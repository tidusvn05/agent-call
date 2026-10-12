//! One-shot Rust adapters for Claude, Codex, Devin, and OpenCode v2 CLIs.
//!
//! Each call runs a fresh CLI process and returns the completed assistant text.
//! Authentication and model defaults come from the installed CLI unless the
//! caller chooses a model explicitly.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

mod other;
pub use other::{AgentCli, Provider, ProviderError};

pub(crate) use other::validate_request;

const STDERR_LIMIT: usize = 4096;

/// Which tools the agent may use during a run.
///
/// [`ToolPolicy::Default`] keeps each CLI's historical settings (see the README). The other
/// policies never give the agent a shell, file access or code execution: a CLI that cannot
/// enforce a requested policy fails with [`Error::InvalidRequest`] instead of running with more
/// power.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ToolPolicy {
    /// The CLI's previous behaviour (Claude: `bypassPermissions`; Codex: read-only sandbox).
    #[default]
    Default,
    /// Only tools from the request's MCP servers. Supported by Claude and Codex.
    McpOnly,
    /// MCP tools plus the CLI's built-in web search and fetch. Supported by Claude and Codex.
    Web,
}

/// An MCP server (streamable HTTP) the agent may use during a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    /// Server name, used in tool names (`mcp__<name>__…`); ASCII letters, digits, `_` and `-`.
    pub name: String,
    /// Streamable HTTP endpoint.
    pub url: String,
    /// Environment variable holding a bearer token. Put the token itself in [`RunRequest::env`]
    /// so it never appears in a command line or a file.
    pub bearer_token_env: Option<String>,
}

/// Progress reported while a run is going (see [`RunRequest::on_event`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AgentEvent {
    /// A piece of the answer as it is generated (Claude).
    TextDelta(String),
    /// A complete text block or message (Codex, OpenCode; Claude between tool calls).
    Text(String),
    /// The agent started using a tool (`WebSearch`, `mcp__kb__kb_grep`, `web_search`…).
    ToolUse {
        /// Tool name as the CLI reports it.
        name: String,
    },
}

/// Receives [`AgentEvent`]s; called from the task reading the CLI's output, so keep it quick.
#[derive(Clone)]
pub struct EventHandler(pub std::sync::Arc<dyn Fn(AgentEvent) + Send + Sync>);

impl EventHandler {
    /// Wraps a callback.
    pub fn new(f: impl Fn(AgentEvent) + Send + Sync + 'static) -> Self {
        Self(std::sync::Arc::new(f))
    }

    pub(crate) fn emit(&self, event: AgentEvent) {
        (self.0)(event);
    }
}

impl std::fmt::Debug for EventHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EventHandler")
    }
}

/// A single, fresh CLI run.
///
/// Build it with struct update syntax so new optional fields do not break callers:
/// `RunRequest { prompt, cwd, ..RunRequest::default() }`.
#[derive(Debug, Clone)]
pub struct RunRequest {
    pub prompt: String,
    pub cwd: PathBuf,
    /// Model ID for the chosen provider, or `None` for its configured default.
    pub model: Option<String>,
    pub timeout: Duration,
    /// Environment variable names to remove from the child only.
    pub env_remove: Vec<OsString>,
    /// Optional JSON Schema. Claude and Codex enforce it; Devin and OpenCode
    /// leave validation to the caller.
    pub json_schema: Option<Value>,
    /// Which tools the agent may use.
    pub tool_policy: ToolPolicy,
    /// MCP servers for this run. Claude and Codex only; with a policy other than
    /// [`ToolPolicy::Default`], MCP servers from the user's own CLI configuration are ignored.
    pub mcp_servers: Vec<McpServer>,
    /// Extra environment variables for the child only, set after `env_remove` is applied.
    pub env: Vec<(OsString, OsString)>,
    /// Instructions appended to the CLI's own system prompt (Claude: `--append-system-prompt`,
    /// Codex: `developer_instructions`). Devin and OpenCode receive it before the prompt.
    pub system_prompt: Option<String>,
    /// Keep the CLI's session so a later run can resume it (default: ephemeral, as before 0.4).
    pub persist_session: bool,
    /// Continue the session returned earlier in [`RunResult::session_id`]. Implies persistence.
    /// Claude and Codex resume in the same `cwd`; OpenCode by id; Devin rejects it.
    pub resume: Option<String>,
    /// Called with progress while the run goes on (Claude streams text; Codex and OpenCode report
    /// whole messages and tool calls; Devin reports nothing).
    pub on_event: Option<EventHandler>,
}

impl Default for RunRequest {
    /// An empty prompt in the current directory with a five-minute timeout.
    fn default() -> Self {
        Self {
            prompt: String::new(),
            cwd: PathBuf::from("."),
            model: None,
            timeout: Duration::from_secs(300),
            env_remove: Vec::new(),
            json_schema: None,
            tool_policy: ToolPolicy::Default,
            mcp_servers: Vec::new(),
            env: Vec::new(),
            system_prompt: None,
            persist_session: false,
            resume: None,
            on_event: None,
        }
    }
}

impl RunRequest {
    /// The prompt with the system prompt in front, for CLIs without a system prompt option.
    pub(crate) fn prompt_with_system(&self) -> String {
        match self.system_prompt.as_deref().map(str::trim) {
            Some(system) if !system.is_empty() => {
                format!(
                    "<instructions>\n{system}\n</instructions>\n\n{}",
                    self.prompt
                )
            }
            _ => self.prompt.clone(),
        }
    }

    /// Rejects tool policies and MCP servers for CLIs that cannot enforce them.
    pub(crate) fn require_default_tools(&self, cli: &str) -> Result<(), Error> {
        if self.tool_policy != ToolPolicy::Default {
            return Err(Error::InvalidRequest(format!(
                "{cli} cannot enforce tool policy {:?}",
                self.tool_policy
            )));
        }
        if !self.mcp_servers.is_empty() {
            return Err(Error::InvalidRequest(format!(
                "{cli} does not support per-run MCP servers"
            )));
        }
        Ok(())
    }
}

/// Token accounting reported by the CLI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// Result of one CLI run.
#[derive(Debug, Clone)]
pub struct RunResult {
    pub text: String,
    pub session_id: Option<String>,
    /// `None` when the CLI stream did not contain complete usage data.
    pub usage: Option<TokenUsage>,
    pub duration: Duration,
    pub stderr_tail: String,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("CLI executable not found: {0}")]
    NotFound(PathBuf),
    #[error("failed to probe CLI version: {0}")]
    VersionProbe(String),
    #[error("OpenCode v2 required, found: {0}")]
    UnsupportedVersion(String),
    #[error("invalid run request: {0}")]
    InvalidRequest(String),
    #[error("failed to run CLI: {0}")]
    Io(#[from] std::io::Error),
    #[error("CLI timed out after {0:?}")]
    Timeout(Duration),
    #[error("CLI exited with {status:?}: {stderr_tail}")]
    Exit {
        status: Option<i32>,
        stderr_tail: String,
    },
    #[error("agent reported an error: {0}")]
    Agent(String),
    #[error("invalid CLI output: {0}")]
    Protocol(String),
    #[error("agent returned no assistant text")]
    EmptyResponse,
    /// The session to resume does not exist (expired, other machine, other `cwd`).
    #[error("session not found: {0}")]
    SessionNotFound(String),
}

/// A verified OpenCode v2 executable.
#[derive(Debug, Clone)]
pub struct OpenCodeV2 {
    binary: PathBuf,
}

impl OpenCodeV2 {
    /// Find `opencode` on `PATH` and verify that its major version is 2.
    pub fn discover() -> Result<Self, Error> {
        Self::with_binary(PathBuf::from("opencode"))
    }

    /// Use an explicit executable path, useful for isolated deployments and tests.
    pub fn with_binary(binary: impl Into<PathBuf>) -> Result<Self, Error> {
        let binary = binary.into();
        let output = std::process::Command::new(&binary)
            .arg("--version")
            .output()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Error::NotFound(binary.clone())
                } else {
                    Error::VersionProbe(e.to_string())
                }
            })?;
        if !output.status.success() {
            return Err(Error::VersionProbe(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let first = version.trim_start_matches('v');
        if !first.starts_with("2.") {
            return Err(Error::UnsupportedVersion(version));
        }
        Ok(Self { binary })
    }

    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// Run a fresh prompt. Dropping the future terminates its child process.
    pub async fn run(&self, request: RunRequest) -> Result<RunResult, Error> {
        validate_request(&request)?;
        request.require_default_tools("OpenCode")?;
        let started = Instant::now();
        let mut command = Command::new(&self.binary);
        command.args(["run", "--standalone", "--format", "json"]);
        if let Some(session) = request.resume.as_deref() {
            command.arg("--session").arg(session);
        }
        if let Some(model) = request.model.as_deref() {
            if model.trim().is_empty() || !model.contains('/') {
                return Err(Error::InvalidRequest(format!(
                    "model must be provider/model[#variant]: {model}"
                )));
            }
            command.arg("--model").arg(model);
        }
        for name in &request.env_remove {
            command.env_remove(name);
        }
        command.envs(request.env.iter().map(|(k, v)| (k, v)));
        command
            .current_dir(&request.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn()?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let work = async {
            let input = async {
                stdin
                    .write_all(request.prompt_with_system().as_bytes())
                    .await?;
                stdin.shutdown().await?;
                drop(stdin);
                Ok::<(), std::io::Error>(())
            };
            let (input, parsed, stderr_tail, status) = tokio::join!(
                input,
                parse_stdout(stdout, request.on_event.clone()),
                read_stderr_tail(stderr),
                child.wait()
            );
            input?;
            let parsed = parsed?;
            let stderr_tail = stderr_tail?;
            let status = status?;
            if !status.success() {
                return Err(Error::Exit {
                    status: status.code(),
                    stderr_tail,
                });
            }
            if let Some(error) = parsed.error {
                return Err(Error::Agent(error));
            }
            let text = parsed.text_parts.join("\n").trim().to_string();
            if text.is_empty() {
                return Err(Error::EmptyResponse);
            }
            let usage = parsed.usage();
            Ok(RunResult {
                text,
                session_id: parsed.session_id,
                usage,
                duration: started.elapsed(),
                stderr_tail,
            })
        };
        match tokio::time::timeout(request.timeout, work).await {
            Ok(result) => result,
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                Err(Error::Timeout(request.timeout))
            }
        }
    }
}

#[derive(Default)]
struct Parsed {
    text_parts: Vec<String>,
    session_id: Option<String>,
    usage_sum: TokenUsage,
    starts: usize,
    finishes: usize,
    last_reason: Option<String>,
    error: Option<String>,
}

impl Parsed {
    fn usage(&self) -> Option<TokenUsage> {
        (self.finishes > 0
            && self.starts == self.finishes
            && self.last_reason.as_deref() != Some("tool-calls"))
        .then(|| self.usage_sum.clone())
    }
}

async fn parse_stdout(
    stdout: impl AsyncRead + Unpin,
    on_event: Option<EventHandler>,
) -> Result<Parsed, Error> {
    let mut lines = BufReader::new(stdout).lines();
    let mut parsed = Parsed::default();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(&line)
            .map_err(|e| Error::Protocol(format!("{e}; line: {}", tail(&line, 200))))?;
        if let Some(id) = event["sessionID"].as_str() {
            parsed.session_id = Some(id.to_string());
        }
        let part = &event["part"];
        match event["type"].as_str() {
            Some("text") => {
                if let Some(text) = part["text"].as_str()
                    && !text.trim().is_empty()
                {
                    if let Some(h) = &on_event {
                        h.emit(AgentEvent::Text(text.to_string()));
                    }
                    parsed.text_parts.push(text.to_string());
                }
            }
            Some("tool_use") => {
                if let (Some(h), Some(name)) = (&on_event, part["tool"].as_str()) {
                    h.emit(AgentEvent::ToolUse {
                        name: name.to_string(),
                    });
                }
            }
            Some("step_start") => parsed.starts += 1,
            Some("step_finish") => {
                parsed.finishes += 1;
                parsed.last_reason = part["reason"].as_str().map(str::to_string);
                let tokens = &part["tokens"];
                parsed.usage_sum.input += tokens["input"].as_u64().unwrap_or(0);
                parsed.usage_sum.output += tokens["output"].as_u64().unwrap_or(0);
                parsed.usage_sum.reasoning += tokens["reasoning"].as_u64().unwrap_or(0);
                parsed.usage_sum.cache_read += tokens["cache"]["read"].as_u64().unwrap_or(0);
                parsed.usage_sum.cache_write += tokens["cache"]["write"].as_u64().unwrap_or(0);
            }
            Some("error") => {
                let message = event["error"]["data"]["message"]
                    .as_str()
                    .or_else(|| event["error"]["message"].as_str())
                    .or_else(|| event["error"]["name"].as_str())
                    .or_else(|| event["error"].as_str())
                    .unwrap_or("unknown OpenCode error");
                parsed.error = Some(message.to_string());
            }
            _ => {}
        }
    }
    Ok(parsed)
}

async fn read_stderr_tail(mut stderr: impl AsyncRead + Unpin) -> Result<String, std::io::Error> {
    let mut tail_bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let n = stderr.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        tail_bytes.extend_from_slice(&chunk[..n]);
        if tail_bytes.len() > STDERR_LIMIT {
            tail_bytes.drain(..tail_bytes.len() - STDERR_LIMIT);
        }
    }
    Ok(String::from_utf8_lossy(&tail_bytes).to_string())
}

fn tail(s: &str, count: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    chars[chars.len().saturating_sub(count)..].iter().collect()
}
