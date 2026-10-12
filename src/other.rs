//! Native one-shot adapters for the Claude, Codex, and Devin CLIs.

use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::{
    AgentEvent, Error, EventHandler, McpServer, OpenCodeV2, RunRequest, RunResult, TokenUsage,
    ToolPolicy,
};

/// An adapter error with the selected provider preserved for callers.
#[derive(Debug, thiserror::Error)]
#[error("{provider:?} CLI: {source}")]
pub struct ProviderError {
    pub provider: Provider,
    #[source]
    pub source: Error,
}

impl ProviderError {
    /// Process exit code, when the CLI exited unsuccessfully.
    pub fn exit_code(&self) -> Option<i32> {
        match &self.source {
            Error::Exit { status, .. } => *status,
            _ => None,
        }
    }

    /// Captured stderr from a failed CLI process, when available.
    pub fn stderr_tail(&self) -> Option<&str> {
        match &self.source {
            Error::Exit { stderr_tail, .. } => Some(stderr_tail),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Claude,
    Codex,
    Devin,
    OpenCodeV2,
}

impl Provider {
    /// Whether the CLI takes [`RunRequest::system_prompt`] as a real system prompt (Claude and
    /// Codex). Other CLIs receive it in front of the user prompt instead.
    pub fn native_system_prompt(self) -> bool {
        matches!(self, Self::Claude | Self::Codex)
    }

    pub fn executable(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Devin => "devin",
            Self::OpenCodeV2 => "opencode",
        }
    }
}

/// A local CLI adapter. Each `run` starts a fresh process and returns one result.
#[derive(Debug, Clone)]
pub struct AgentCli {
    provider: Provider,
    binary: PathBuf,
}

impl AgentCli {
    /// Discover a CLI while preserving its provider in any error.
    pub fn discover_with_context(provider: Provider) -> Result<Self, ProviderError> {
        Self::discover(provider).map_err(|source| ProviderError { provider, source })
    }

    /// Use an explicit CLI path while preserving its provider in any error.
    pub fn with_binary_with_context(
        provider: Provider,
        binary: impl Into<PathBuf>,
    ) -> Result<Self, ProviderError> {
        Self::with_binary(provider, binary).map_err(|source| ProviderError { provider, source })
    }

    pub fn discover(provider: Provider) -> Result<Self, Error> {
        Self::with_binary(provider, provider.executable())
    }

    pub fn with_binary(provider: Provider, binary: impl Into<PathBuf>) -> Result<Self, Error> {
        let binary = binary.into();
        if provider == Provider::OpenCodeV2 {
            OpenCodeV2::with_binary(binary.clone())?;
        } else {
            let output = std::process::Command::new(&binary)
                .arg("--version")
                .output()
                .map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => Error::NotFound(binary.clone()),
                    _ => Error::VersionProbe(e.to_string()),
                })?;
            if !output.status.success() {
                return Err(Error::VersionProbe(format!(
                    "{} --version exited {:?}",
                    provider.executable(),
                    output.status.code()
                )));
            }
        }
        Ok(Self { provider, binary })
    }

    pub fn provider(&self) -> Provider {
        self.provider
    }

    /// Whether this CLI supports [`RunRequest::system_prompt`] natively; see
    /// [`Provider::native_system_prompt`].
    pub fn native_system_prompt(&self) -> bool {
        self.provider.native_system_prompt()
    }

    /// Run a request while preserving provider, exit code, and stderr in errors.
    pub async fn run_with_context(&self, request: RunRequest) -> Result<RunResult, ProviderError> {
        self.run(request).await.map_err(|source| ProviderError {
            provider: self.provider,
            source,
        })
    }

    pub async fn run(&self, request: RunRequest) -> Result<RunResult, Error> {
        validate_request(&request)?;
        match self.provider {
            Provider::OpenCodeV2 => {
                OpenCodeV2 {
                    binary: self.binary.clone(),
                }
                .run(request)
                .await
            }
            Provider::Claude => self.run_claude(request).await,
            Provider::Codex => self.run_codex(request).await,
            Provider::Devin => self.run_devin(request).await,
        }
    }

    async fn run_claude(&self, request: RunRequest) -> Result<RunResult, Error> {
        let streaming = request.on_event.is_some();
        let mut command = Command::new(&self.binary);
        command.args(["-p", "--output-format"]);
        if streaming {
            command.args(["stream-json", "--verbose", "--include-partial-messages"]);
        } else {
            command.arg("json");
        }
        command.args(["--setting-sources", "local"]);
        match (&request.resume, request.persist_session) {
            (Some(session), _) => {
                command.arg("--resume").arg(session);
            }
            (None, false) => {
                command.arg("--no-session-persistence");
            }
            (None, true) => {}
        }
        claude_tools(&mut command, &request);
        if let Some(system) = request
            .system_prompt
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            command.arg("--append-system-prompt").arg(system);
        }
        if let Some(model) = request.model.as_deref() {
            let (name, effort) = split_effort(model, &["low", "medium", "high", "xhigh", "max"])?;
            command.arg("--model").arg(name);
            if let Some(effort) = effort {
                command.arg("--effort").arg(effort);
            }
        }
        if let Some(mut schema) = request.json_schema.clone() {
            strip_schema_uri(&mut schema);
            command.arg("--json-schema").arg(schema.to_string());
        }
        configure(&mut command, &request, true);
        if request.tool_policy != ToolPolicy::Default {
            // Claude Code inlines `@path` mentions from the prompt (any readable path, also
            // outside `cwd`) before the model runs, independent of `--tools`. Set after the
            // caller's environment so it cannot be overridden.
            command.env("CLAUDE_CODE_DISABLE_ATTACHMENTS", "1");
        }
        let handler = request.on_event.clone();
        let on_line = move |line: &str| {
            if let Some(handler) = &handler {
                claude_event(handler, line);
            }
        };
        let (output, duration) = capture(
            command,
            Some(&request.prompt),
            request.timeout,
            streaming.then_some(&on_line as &(dyn Fn(&str) + Send + Sync)),
        )
        .await?;
        let stderr_tail = stderr_tail(&output.stderr);
        if let Some(session) = &request.resume
            && !output.status.success()
            && stderr_tail.contains("No conversation found")
        {
            return Err(Error::SessionNotFound(session.clone()));
        }
        let parsed = if streaming {
            // The last `result` line carries the same envelope as `--output-format json`.
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .rev()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .find(|v| v["type"] == "result")
                .ok_or_else(|| serde_json::Error::io(std::io::Error::other("no result line")))
        } else {
            serde_json::from_slice(&output.stdout)
        };
        let envelope: Value = match parsed {
            Ok(value) => value,
            Err(error) => {
                check_status(&output, &stderr_tail)?;
                return Err(Error::Protocol(format!("Claude JSON envelope: {error}")));
            }
        };
        let error = if envelope["is_error"].as_bool() == Some(true) {
            let details = envelope["errors"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .filter(|s| !s.is_empty());
            Some(
                details
                    .or_else(|| envelope["result"].as_str().map(str::to_string))
                    .unwrap_or_else(|| "unknown Claude error".into()),
            )
        } else {
            None
        };
        if let Some(error) = error {
            return Err(Error::Agent(error));
        }
        check_status(&output, &stderr_tail)?;
        let text = envelope
            .get("structured_output")
            .filter(|v| !v.is_null())
            .map(Value::to_string)
            .or_else(|| envelope["result"].as_str().map(str::to_string))
            .unwrap_or_default();
        let usage = envelope
            .get("usage")
            .filter(|v| v.is_object())
            .map(|u| TokenUsage {
                input: u["input_tokens"].as_u64().unwrap_or(0),
                output: u["output_tokens"].as_u64().unwrap_or(0),
                cache_read: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
                cache_write: u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
                ..Default::default()
            });
        result(
            text,
            envelope["session_id"].as_str(),
            usage,
            duration,
            stderr_tail,
        )
    }

    async fn run_codex(&self, request: RunRequest) -> Result<RunResult, Error> {
        let out_file = tempfile::NamedTempFile::new()?;
        let schema_file = if let Some(mut schema) = request.json_schema.clone() {
            normalize_codex_schema(&mut schema);
            let file = tempfile::NamedTempFile::new()?;
            std::fs::write(file.path(), schema.to_string())?;
            Some(file)
        } else {
            None
        };
        let mut command = Command::new(&self.binary);
        command.arg("exec");
        if request.resume.is_some() {
            command.arg("resume");
        }
        // `-c sandbox_mode` works for `exec` and `exec resume` (which has no `-s`).
        command.args(["--skip-git-repo-check", "-c", "sandbox_mode=\"read-only\""]);
        if request.resume.is_none() {
            command.args(["--color", "never"]);
            if !request.persist_session {
                command.arg("--ephemeral");
            }
        }
        command
            .args([
                "--disable",
                "hooks",
                "-c",
                "project_doc_max_bytes=0",
                "--json",
                "-o",
            ])
            .arg(out_file.path());
        if let Some(model) = request.model.as_deref() {
            let (name, effort) =
                split_effort(model, &["low", "medium", "high", "xhigh", "max", "ultra"])?;
            command.arg("-m").arg(name);
            if let Some(effort) = effort {
                command
                    .arg("-c")
                    .arg(format!("model_reasoning_effort=\"{effort}\""));
            }
        }
        if let Some(file) = &schema_file {
            command.arg("--output-schema").arg(file.path());
        }
        codex_tools(&mut command, &request);
        if let Some(system) = request
            .system_prompt
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            command
                .arg("-c")
                .arg(format!("developer_instructions={}", toml_string(system)));
        }
        if let Some(session) = &request.resume {
            command.arg(session);
        }
        command.arg("-");
        configure(&mut command, &request, true);
        let handler = request.on_event.clone();
        let on_line = move |line: &str| {
            if let Some(handler) = &handler {
                codex_event(handler, line);
            }
        };
        let (output, duration) = capture(
            command,
            Some(&request.prompt),
            request.timeout,
            request
                .on_event
                .is_some()
                .then_some(&on_line as &(dyn Fn(&str) + Send + Sync)),
        )
        .await?;
        let stderr_tail = stderr_tail(&output.stderr);
        // Only stderr: stdout carries the agent's own text, which may quote anything.
        if let Some(session) = &request.resume
            && !output.status.success()
            && stderr_tail.contains("no rollout found")
        {
            return Err(Error::SessionNotFound(session.clone()));
        }
        check_status(&output, &stderr_tail)?;
        let mut text = None;
        let mut session_id = None;
        let mut usage = None;
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let event: Value = serde_json::from_str(line)
                .map_err(|e| Error::Protocol(format!("Codex JSONL: {e}")))?;
            match event["type"].as_str() {
                Some("thread.started") => {
                    session_id = event["thread_id"].as_str().map(str::to_string)
                }
                Some("item.completed") if event["item"]["type"] == "agent_message" => {
                    text = event["item"]["text"].as_str().map(str::to_string);
                }
                Some("turn.completed") => {
                    let u = &event["usage"];
                    usage = Some(TokenUsage {
                        input: u["input_tokens"].as_u64().unwrap_or(0),
                        output: u["output_tokens"].as_u64().unwrap_or(0),
                        reasoning: u["reasoning_output_tokens"].as_u64().unwrap_or(0),
                        ..Default::default()
                    });
                }
                Some("turn.failed" | "error") => {
                    return Err(Error::Agent(
                        event["error"]["message"]
                            .as_str()
                            .or_else(|| event["message"].as_str())
                            .unwrap_or("unknown Codex error")
                            .to_string(),
                    ));
                }
                _ => {}
            }
        }
        let from_file = std::fs::read_to_string(out_file.path())?;
        result(
            if from_file.trim().is_empty() {
                text.unwrap_or_default()
            } else {
                from_file
            },
            session_id.as_deref(),
            usage,
            duration,
            stderr_tail,
        )
    }

    async fn run_devin(&self, request: RunRequest) -> Result<RunResult, Error> {
        request.require_default_tools("Devin")?;
        if request.resume.is_some() {
            return Err(Error::InvalidRequest("Devin cannot resume sessions".into()));
        }
        let prompt_file = tempfile::NamedTempFile::new()?;
        std::fs::write(prompt_file.path(), request.prompt_with_system())?;
        let mut command = Command::new(&self.binary);
        command
            .arg("-p")
            .arg("--prompt-file")
            .arg(prompt_file.path())
            .args([
                "--respect-workspace-trust",
                "false",
                "--permission-mode",
                "auto",
            ]);
        if let Some(model) = &request.model {
            command.arg("--model").arg(model);
        }
        configure(&mut command, &request, false);
        let (output, duration) = capture(command, None, request.timeout, None).await?;
        let stderr_tail = stderr_tail(&output.stderr);
        check_status(&output, &stderr_tail)?;
        result(
            String::from_utf8_lossy(&output.stdout).to_string(),
            None,
            None,
            duration,
            stderr_tail,
        )
    }
}

/// Claude `stream-json` line → event.
fn claude_event(handler: &EventHandler, line: &str) {
    let Ok(event) = serde_json::from_str::<Value>(line) else {
        return;
    };
    match event["type"].as_str() {
        Some("stream_event") => {
            let delta = &event["event"]["delta"];
            if delta["type"] == "text_delta"
                && let Some(text) = delta["text"].as_str()
            {
                handler.emit(AgentEvent::TextDelta(text.to_string()));
            }
        }
        Some("assistant") => {
            for block in event["message"]["content"].as_array().into_iter().flatten() {
                if block["type"] == "tool_use"
                    && let Some(name) = block["name"].as_str()
                {
                    handler.emit(AgentEvent::ToolUse {
                        name: name.to_string(),
                    });
                }
            }
        }
        _ => {}
    }
}

/// Codex JSONL line → event.
fn codex_event(handler: &EventHandler, line: &str) {
    let Ok(event) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let item = &event["item"];
    match (event["type"].as_str(), item["type"].as_str()) {
        (Some("item.started"), Some(kind)) if kind != "agent_message" && kind != "reasoning" => {
            let name = item["tool"]
                .as_str()
                .or_else(|| item["name"].as_str())
                .unwrap_or(kind);
            handler.emit(AgentEvent::ToolUse {
                name: name.to_string(),
            });
        }
        (Some("item.completed"), Some("agent_message")) => {
            if let Some(text) = item["text"].as_str() {
                handler.emit(AgentEvent::Text(text.to_string()));
            }
        }
        _ => {}
    }
}

pub(crate) fn validate_request(request: &RunRequest) -> Result<(), Error> {
    if request.prompt.trim().is_empty() {
        return Err(Error::InvalidRequest("prompt is empty".into()));
    }
    if !request.cwd.is_dir() {
        return Err(Error::InvalidRequest(format!(
            "working directory does not exist: {}",
            request.cwd.display()
        )));
    }
    if let Some(session) = &request.resume
        && (!is_identifier(session) || session.starts_with('-'))
    {
        // Session ids end up as command-line arguments (a positional one for Codex).
        return Err(Error::InvalidRequest(format!(
            "session id must use ASCII letters, digits, `_` or `-`: {session:?}"
        )));
    }
    let mut names = std::collections::HashSet::new();
    for server in &request.mcp_servers {
        validate_mcp_server(server)?;
        if !names.insert(server.name.as_str()) {
            return Err(Error::InvalidRequest(format!(
                "duplicate MCP server name: {}",
                server.name
            )));
        }
    }
    Ok(())
}

fn is_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn validate_mcp_server(server: &McpServer) -> Result<(), Error> {
    if !is_identifier(&server.name) {
        return Err(Error::InvalidRequest(format!(
            "MCP server name must use ASCII letters, digits, `_` or `-`: {:?}",
            server.name
        )));
    }
    if !(server.url.starts_with("http://") || server.url.starts_with("https://"))
        || server.url.chars().any(|c| c.is_whitespace() || c == '"')
    {
        return Err(Error::InvalidRequest(format!(
            "MCP server URL must be http(s) without spaces or quotes: {}",
            server.url
        )));
    }
    if let Some(var) = &server.bearer_token_env
        && !(is_identifier(var) && !var.contains('-'))
    {
        return Err(Error::InvalidRequest(format!(
            "bearer token variable must be a plain environment variable name: {var:?}"
        )));
    }
    Ok(())
}

/// Claude Code built-in tools for each restricted policy. `--tools` is a whitelist, so tools
/// added in later Claude Code versions stay off.
const CLAUDE_WEB_TOOLS: &str = "WebSearch,WebFetch";

/// Applies the tool policy and MCP servers to a Claude Code command line.
fn claude_tools(command: &mut Command, request: &RunRequest) {
    let restricted = request.tool_policy != ToolPolicy::Default;
    if !restricted {
        command.args(["--permission-mode", "bypassPermissions"]);
    }
    if !request.mcp_servers.is_empty() {
        let servers: serde_json::Map<String, Value> = request
            .mcp_servers
            .iter()
            .map(|s| {
                let mut server = serde_json::json!({"type": "http", "url": s.url});
                if let Some(var) = &s.bearer_token_env {
                    // Claude Code expands `${VAR}` from the child environment.
                    server["headers"] =
                        serde_json::json!({"Authorization": format!("Bearer ${{{var}}}")});
                }
                (s.name.clone(), server)
            })
            .collect();
        command
            .arg("--mcp-config")
            .arg(serde_json::json!({"mcpServers": servers}).to_string());
    }
    if restricted {
        // Never load the user's own MCP servers into a restricted run.
        command.arg("--strict-mcp-config");
        // Untrusted prompts must not invoke skills or custom commands with `/name`.
        command.arg("--disable-slash-commands");
        let builtin = match request.tool_policy {
            ToolPolicy::Web => CLAUDE_WEB_TOOLS,
            _ => "",
        };
        command.arg("--tools").arg(builtin);
        let mut allowed: Vec<String> = request
            .mcp_servers
            .iter()
            .map(|s| format!("mcp__{}", s.name))
            .collect();
        if request.tool_policy == ToolPolicy::Web {
            allowed.extend(CLAUDE_WEB_TOOLS.split(',').map(str::to_string));
        }
        if !allowed.is_empty() {
            command.arg("--allowedTools").arg(allowed.join(","));
        }
        // Anything not allowed above is denied instead of prompting.
        command.args(["--permission-mode", "dontAsk"]);
    }
}

/// Codex features that run code, read files or reach beyond MCP and web search. Turned off for
/// every restricted policy; the read-only sandbox stays on as a second layer.
const CODEX_RESTRICTED_FEATURES: [&str; 8] = [
    "shell_tool",
    "unified_exec",
    "view_image",
    "browser_use",
    "computer_use",
    "in_app_browser",
    "apps",
    // Plugins can bring their own MCP servers and skills from the user's configuration.
    "plugins",
];

/// Applies the tool policy and MCP servers to a Codex command line.
fn codex_tools(command: &mut Command, request: &RunRequest) {
    let restricted = request.tool_policy != ToolPolicy::Default;
    if restricted {
        for feature in CODEX_RESTRICTED_FEATURES {
            command.arg("--disable").arg(feature);
        }
        let search = if request.tool_policy == ToolPolicy::Web {
            "live"
        } else {
            "disabled"
        };
        command.arg("-c").arg(format!("web_search=\"{search}\""));
        // Drop the user's own MCP servers; the request's servers are added below.
        command.arg("-c").arg("mcp_servers={}");
    }
    for server in &request.mcp_servers {
        command.arg("-c").arg(format!(
            "mcp_servers.{}.url={}",
            server.name,
            toml_string(&server.url)
        ));
        if let Some(var) = &server.bearer_token_env {
            command.arg("-c").arg(format!(
                "mcp_servers.{}.bearer_token_env_var={}",
                server.name,
                toml_string(var)
            ));
        }
    }
}

/// A TOML basic string. JSON string escaping is a subset of TOML's, except that TOML also
/// forbids a raw DEL character.
fn toml_string(value: &str) -> String {
    Value::String(value.to_owned())
        .to_string()
        .replace('\u{7f}', "\\u007F")
}

fn split_effort<'a>(model: &'a str, valid: &[&str]) -> Result<(&'a str, Option<&'a str>), Error> {
    if model.trim().is_empty() {
        return Err(Error::InvalidRequest("model is empty".into()));
    }
    if let Some((name, effort)) = model.rsplit_once('@') {
        if name.is_empty() || !valid.contains(&effort) {
            return Err(Error::InvalidRequest(format!(
                "unsupported model effort: {model}"
            )));
        }
        Ok((name, Some(effort)))
    } else {
        Ok((model, None))
    }
}

fn configure(command: &mut Command, request: &RunRequest, stdin: bool) {
    command
        .current_dir(&request.cwd)
        .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for name in &request.env_remove {
        command.env_remove(name);
    }
    command.envs(request.env.iter().map(|(k, v)| (k, v)));
}

/// Called with each stdout line as it arrives (without the newline).
type LineHandler<'a> = Option<&'a (dyn Fn(&str) + Send + Sync)>;

async fn capture(
    mut command: Command,
    prompt: Option<&str>,
    timeout: Duration,
    on_line: LineHandler<'_>,
) -> Result<(Output, Duration), Error> {
    let started = Instant::now();
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let work = async {
        let write = async {
            if let (Some(prompt), Some(mut stdin)) = (prompt, stdin.take()) {
                stdin.write_all(prompt.as_bytes()).await?;
                stdin.shutdown().await?;
            }
            Ok::<(), std::io::Error>(())
        };
        let read_out = async {
            let mut bytes = Vec::new();
            let mut reader = BufReader::new(stdout);
            loop {
                let start = bytes.len();
                if reader.read_until(b'\n', &mut bytes).await? == 0 {
                    break;
                }
                if let Some(on_line) = on_line {
                    on_line(String::from_utf8_lossy(&bytes[start..]).trim_end());
                }
            }
            Ok::<_, std::io::Error>(bytes)
        };
        let read_err = async {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).await?;
            Ok::<_, std::io::Error>(bytes)
        };
        let (written, out, err, status) = tokio::join!(write, read_out, read_err, child.wait());
        let output = Output {
            status: status?,
            stdout: out?,
            stderr: err?,
        };
        if let Err(error) = written
            && output.status.success()
        {
            return Err(Error::Io(error));
        }
        Ok((output, started.elapsed()))
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(value) => value,
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(Error::Timeout(timeout))
        }
    }
}

fn check_status(output: &Output, stderr_tail: &str) -> Result<(), Error> {
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Exit {
            status: output.status.code(),
            stderr_tail: stderr_tail.into(),
        })
    }
}

fn result(
    text: String,
    session_id: Option<&str>,
    usage: Option<TokenUsage>,
    duration: Duration,
    stderr_tail: String,
) -> Result<RunResult, Error> {
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err(Error::EmptyResponse);
    }
    Ok(RunResult {
        text,
        session_id: session_id.map(str::to_string),
        usage,
        duration,
        stderr_tail,
    })
}

fn stderr_tail(bytes: &[u8]) -> String {
    let bytes = &bytes[bytes.len().saturating_sub(4096)..];
    String::from_utf8_lossy(bytes).to_string()
}

fn strip_schema_uri(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("$schema");
            for item in map.values_mut() {
                strip_schema_uri(item);
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_schema_uri(item);
            }
        }
        _ => {}
    }
}

fn normalize_codex_schema(value: &mut Value) {
    let map = match value {
        Value::Object(map) => map,
        Value::Array(items) => {
            for item in items {
                normalize_codex_schema(item);
            }
            return;
        }
        _ => return,
    };
    map.remove("$schema");
    if map.contains_key("$ref") && map.len() > 1 {
        let reference = map.remove("$ref").expect("reference exists");
        map.clear();
        map.insert("$ref".into(), reference);
    }
    if let Some(Value::Array(variants)) = map.remove("oneOf") {
        let constants: Option<Vec<Value>> =
            variants.iter().map(|v| v.get("const").cloned()).collect();
        if let Some(constants) = constants {
            let mut types: Vec<&str> = variants
                .iter()
                .filter_map(|v| v.get("type").and_then(Value::as_str))
                .collect();
            types.sort_unstable();
            types.dedup();
            if let [ty] = types.as_slice() {
                map.insert("type".into(), Value::String((*ty).into()));
            }
            map.insert("enum".into(), Value::Array(constants));
        } else {
            map.insert("anyOf".into(), Value::Array(variants));
        }
    }
    for child in map.values_mut() {
        normalize_codex_schema(child);
    }
    if map.get("type").and_then(Value::as_str) == Some("object") || map.contains_key("properties") {
        let required: std::collections::HashSet<String> = map
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        if let Some(Value::Object(properties)) = map.get_mut("properties") {
            let all = properties.keys().cloned().map(Value::String).collect();
            for (name, spec) in properties.iter_mut() {
                if !required.contains(name) && !nullable(spec) {
                    let previous = spec.take();
                    *spec = serde_json::json!({"anyOf":[previous,{"type":"null"}]});
                }
            }
            map.insert("required".into(), Value::Array(all));
        }
        map.insert("additionalProperties".into(), Value::Bool(false));
    }
}

fn nullable(value: &Value) -> bool {
    value["type"] == "null"
        || value["type"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v == "null"))
        || value["anyOf"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v["type"] == "null"))
}

#[cfg(test)]
mod tests {
    use super::toml_string;

    #[test]
    fn toml_string_escapes_control_characters() {
        assert_eq!(
            toml_string("a\"b\\c\nd\u{1}e\u{7f}"),
            r#""a\"b\\c\nd\u0001e\u007F""#
        );
    }
}
