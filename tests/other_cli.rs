#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use agent_call::{
    AgentCli, AgentEvent, Error, EventHandler, McpServer, Provider, RunRequest, ToolPolicy,
};
use std::sync::{Arc, Mutex};

fn fake_cli(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!("#!/bin/sh\nif [ \"$1\" = --version ]; then echo 1.0.0; exit 0; fi\n{body}\n"),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    // Some filesystems briefly report ETXTBSY immediately after creating an
    // executable shell script while tests run in parallel.
    std::thread::sleep(Duration::from_millis(10));
    path
}

fn request(dir: &Path) -> RunRequest {
    RunRequest {
        prompt: "hello\nworld".into(),
        cwd: dir.into(),
        model: None,
        timeout: Duration::from_secs(2),
        env_remove: vec![],
        json_schema: None,
        ..RunRequest::default()
    }
}

#[tokio::test]
async fn claude_parses_structured_output_and_usage() {
    let dir = tempfile::tempdir().unwrap();
    let binary = fake_cli(
        dir.path(),
        "claude",
        r#"
[ "$1" = -p ] || exit 2
[ "$(cat)" = 'hello
world' ] || exit 3
printf '%s\n' '{"is_error":false,"structured_output":{"ok":true},"session_id":"s1","usage":{"input_tokens":2,"output_tokens":3,"cache_read_input_tokens":4}}'
"#,
    );
    let cli = AgentCli::with_binary(Provider::Claude, binary).unwrap();
    let result = cli.run(request(dir.path())).await.unwrap();
    assert_eq!(result.text, "{\"ok\":true}");
    assert_eq!(result.session_id.as_deref(), Some("s1"));
    assert_eq!(result.usage.unwrap().cache_read, 4);
}

#[tokio::test]
async fn codex_reads_final_file_and_usage() {
    let dir = tempfile::tempdir().unwrap();
    let binary = fake_cli(
        dir.path(),
        "codex",
        r#"
[ "$1" = exec ] || exit 2
out=''
while [ "$#" -gt 0 ]; do
  if [ "$1" = -o ]; then shift; out="$1"; fi
  shift
done
[ "$(cat)" = 'hello
world' ] || exit 3
printf 'final answer' > "$out"
printf '%s\n' '{"type":"thread.started","thread_id":"thread1"}' '{"type":"turn.completed","usage":{"input_tokens":8,"output_tokens":5,"reasoning_output_tokens":2}}'
"#,
    );
    let cli = AgentCli::with_binary(Provider::Codex, binary).unwrap();
    let result = cli.run(request(dir.path())).await.unwrap();
    assert_eq!(result.text, "final answer");
    assert_eq!(result.session_id.as_deref(), Some("thread1"));
    assert_eq!(result.usage.unwrap().reasoning, 2);
}

#[tokio::test]
async fn devin_reads_prompt_file() {
    let dir = tempfile::tempdir().unwrap();
    let binary = fake_cli(
        dir.path(),
        "devin",
        r#"
[ "$1" = -p ] && [ "$2" = --prompt-file ] || exit 2
[ "$(cat "$3")" = 'hello
world' ] || exit 3
echo 'Devin answer'
"#,
    );
    let cli = AgentCli::with_binary(Provider::Devin, binary).unwrap();
    let result = cli.run(request(dir.path())).await.unwrap();
    assert_eq!(result.text, "Devin answer");
    assert!(result.usage.is_none());
}

#[tokio::test]
async fn claude_surfaces_error_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let binary = fake_cli(
        dir.path(),
        "claude-error",
        "cat >/dev/null\necho '{\"is_error\":true,\"result\":\"bad model\"}'",
    );
    let cli = AgentCli::with_binary(Provider::Claude, binary).unwrap();
    assert!(matches!(cli.run(request(dir.path())).await, Err(Error::Agent(s)) if s == "bad model"));
}

#[tokio::test]
async fn claude_passes_model_effort_and_clean_schema() {
    let dir = tempfile::tempdir().unwrap();
    let binary = fake_cli(
        dir.path(),
        "claude-schema",
        r#"
schema=''; model=''; effort=''
while [ "$#" -gt 0 ]; do
  case "$1" in
    --json-schema) shift; schema="$1" ;;
    --model) shift; model="$1" ;;
    --effort) shift; effort="$1" ;;
  esac
  shift
done
[ "$model" = sonnet ] && [ "$effort" = high ] || exit 2
case "$schema" in *'$schema'*) exit 3;; esac
[ -n "$schema" ] || exit 4
cat >/dev/null
echo '{"result":"ok"}'
"#,
    );
    let cli = AgentCli::with_binary(Provider::Claude, binary).unwrap();
    let mut req = request(dir.path());
    req.model = Some("sonnet@high".into());
    req.json_schema = Some(serde_json::json!({"$schema":"draft","type":"object"}));
    assert_eq!(cli.run(req).await.unwrap().text, "ok");
}

#[tokio::test]
async fn codex_normalizes_schema_and_rejects_bad_effort() {
    let dir = tempfile::tempdir().unwrap();
    let binary = fake_cli(
        dir.path(),
        "codex-schema",
        r#"
out=''; schema=''
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) shift; out="$1" ;;
    --output-schema) shift; schema="$1" ;;
  esac
  shift
done
[ -n "$schema" ] || exit 2
grep -q '"additionalProperties":false' "$schema" || exit 3
grep -q '"required":\["maybe"\]' "$schema" || exit 4
cat >/dev/null
echo ok > "$out"
echo '{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}'
"#,
    );
    let cli = AgentCli::with_binary(Provider::Codex, binary).unwrap();
    let mut req = request(dir.path());
    req.model = Some("gpt-5.6-sol@turbo".into());
    assert!(matches!(cli.run(req).await, Err(Error::InvalidRequest(_))));
    let mut req = request(dir.path());
    req.json_schema = Some(serde_json::json!({
        "type":"object", "properties":{"maybe":{"type":"string"}}
    }));
    assert_eq!(cli.run(req).await.unwrap().text, "ok");
}

#[tokio::test]
async fn contextual_error_preserves_provider_exit_code_and_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let binary = fake_cli(dir.path(), "devin-error", "echo 'bad model' >&2\nexit 17");
    let cli = AgentCli::with_binary(Provider::Devin, binary).unwrap();
    let error = cli.run_with_context(request(dir.path())).await.unwrap_err();
    assert_eq!(error.provider, Provider::Devin);
    assert_eq!(error.exit_code(), Some(17));
    assert_eq!(error.stderr_tail(), Some("bad model\n"));
    assert!(matches!(
        error.source,
        Error::Exit {
            status: Some(17),
            ..
        }
    ));
}

#[tokio::test]
async fn contextual_discovery_error_preserves_provider() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing-codex");
    let error = AgentCli::with_binary_with_context(Provider::Codex, missing).unwrap_err();
    assert_eq!(error.provider, Provider::Codex);
    assert!(matches!(error.source, Error::NotFound(_)));
}

#[tokio::test]
async fn other_adapters_enforce_timeout() {
    let dir = tempfile::tempdir().unwrap();
    for provider in [Provider::Claude, Provider::Codex, Provider::Devin] {
        let binary = fake_cli(dir.path(), provider.executable(), "exec sleep 5");
        let cli = AgentCli::with_binary(provider, binary).unwrap();
        let mut req = request(dir.path());
        req.timeout = Duration::from_millis(50);
        let error = cli.run_with_context(req).await.unwrap_err();
        assert_eq!(error.provider, provider);
        assert!(matches!(error.source, Error::Timeout(_)));
        assert_eq!(error.exit_code(), None);
        assert_eq!(error.stderr_tail(), None);
    }
}

/// A fake CLI that records its arguments (one per line) and selected environment variables.
fn recording_cli(dir: &Path, name: &str, reply: &str) -> std::path::PathBuf {
    fake_cli(
        dir,
        name,
        &format!(
            r#"for a in "$@"; do printf '%s\n' "$a"; done > "{args}"
printf '%s' "${{KB_TOKEN:-unset}}" > "{env}"
printf '%s' "${{CLAUDE_CODE_DISABLE_ATTACHMENTS:-unset}}" > "{attachments}"
cat > "{stdin}"
out=''
prev=''
for a in "$@"; do [ "$prev" = -o ] && out="$a"; prev="$a"; done
[ -n "$out" ] && printf 'final' > "$out"
printf '%s\n' '{reply}'"#,
            args = dir.join("args").display(),
            env = dir.join("env").display(),
            attachments = dir.join("attachments").display(),
            stdin = dir.join("stdin").display(),
        ),
    )
}

fn args(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("args"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

fn after<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
    args.windows(2)
        .filter(|w| w[0] == flag)
        .map(|w| w[1].as_str())
        .collect()
}

fn kb_server() -> McpServer {
    McpServer {
        name: "kb".into(),
        url: "http://127.0.0.1:9/kb/mcp".into(),
        bearer_token_env: Some("KB_TOKEN".into()),
    }
}

fn restricted(dir: &Path, policy: ToolPolicy) -> RunRequest {
    RunRequest {
        tool_policy: policy,
        mcp_servers: vec![kb_server()],
        env: vec![("KB_TOKEN".into(), "secret-token".into())],
        system_prompt: Some("Be brief.".into()),
        ..request(dir)
    }
}

const CLAUDE_OK: &str = r#"{"is_error":false,"result":"ok"}"#;
const CODEX_OK: &str = r#"{"type":"turn.completed","usage":{}}"#;

#[tokio::test]
async fn claude_default_policy_keeps_previous_flags() {
    let dir = tempfile::tempdir().unwrap();
    let cli = AgentCli::with_binary(
        Provider::Claude,
        recording_cli(dir.path(), "claude", CLAUDE_OK),
    )
    .unwrap();
    cli.run(request(dir.path())).await.unwrap();
    let a = args(dir.path());
    assert_eq!(after(&a, "--permission-mode"), ["bypassPermissions"]);
    assert!(!a.iter().any(|x| x == "--tools"
        || x == "--mcp-config"
        || x == "--append-system-prompt"
        || x == "--disable-slash-commands"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("attachments")).unwrap(),
        "unset"
    );
}

#[tokio::test]
async fn claude_mcp_only() {
    let dir = tempfile::tempdir().unwrap();
    let cli = AgentCli::with_binary(
        Provider::Claude,
        recording_cli(dir.path(), "claude", CLAUDE_OK),
    )
    .unwrap();
    cli.run(restricted(dir.path(), ToolPolicy::McpOnly))
        .await
        .unwrap();
    let a = args(dir.path());
    assert_eq!(
        after(&a, "--permission-mode"),
        ["dontAsk"],
        "never bypassPermissions"
    );
    assert_eq!(after(&a, "--tools"), [""]);
    assert_eq!(after(&a, "--allowedTools"), ["mcp__kb"]);
    assert!(a.iter().any(|x| x == "--strict-mcp-config"));
    assert!(a.iter().any(|x| x == "--disable-slash-commands"));
    let config: serde_json::Value = serde_json::from_str(after(&a, "--mcp-config")[0]).unwrap();
    assert_eq!(
        config["mcpServers"]["kb"]["url"],
        "http://127.0.0.1:9/kb/mcp"
    );
    assert_eq!(
        config["mcpServers"]["kb"]["headers"]["Authorization"],
        "Bearer ${KB_TOKEN}"
    );
    assert_eq!(after(&a, "--append-system-prompt"), ["Be brief."]);
    assert!(
        !a.iter().any(|x| x.contains("secret-token")),
        "token never on the command line"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("env")).unwrap(),
        "secret-token"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("attachments")).unwrap(),
        "1",
        "`@path` mentions must not be inlined"
    );
}

#[tokio::test]
async fn claude_restricted_attachment_guard_wins_over_caller_env() {
    let dir = tempfile::tempdir().unwrap();
    let cli = AgentCli::with_binary(
        Provider::Claude,
        recording_cli(dir.path(), "claude", CLAUDE_OK),
    )
    .unwrap();
    let mut req = restricted(dir.path(), ToolPolicy::Web);
    req.env
        .push(("CLAUDE_CODE_DISABLE_ATTACHMENTS".into(), "0".into()));
    req.env_remove
        .push("CLAUDE_CODE_DISABLE_ATTACHMENTS".into());
    cli.run(req).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("attachments")).unwrap(),
        "1"
    );
}

#[test]
fn native_system_prompt_support() {
    assert!(Provider::Claude.native_system_prompt());
    assert!(Provider::Codex.native_system_prompt());
    assert!(!Provider::Devin.native_system_prompt());
    assert!(!Provider::OpenCodeV2.native_system_prompt());
}

#[tokio::test]
async fn claude_web() {
    let dir = tempfile::tempdir().unwrap();
    let cli = AgentCli::with_binary(
        Provider::Claude,
        recording_cli(dir.path(), "claude", CLAUDE_OK),
    )
    .unwrap();
    cli.run(restricted(dir.path(), ToolPolicy::Web))
        .await
        .unwrap();
    let a = args(dir.path());
    assert_eq!(after(&a, "--tools"), ["WebSearch,WebFetch"]);
    assert_eq!(after(&a, "--allowedTools"), ["mcp__kb,WebSearch,WebFetch"]);
    assert_eq!(after(&a, "--permission-mode"), ["dontAsk"]);
}

#[tokio::test]
async fn codex_restricted_policies() {
    for (policy, search) in [(ToolPolicy::McpOnly, "disabled"), (ToolPolicy::Web, "live")] {
        let dir = tempfile::tempdir().unwrap();
        let cli = AgentCli::with_binary(
            Provider::Codex,
            recording_cli(dir.path(), "codex", CODEX_OK),
        )
        .unwrap();
        cli.run(restricted(dir.path(), policy)).await.unwrap();
        let a = args(dir.path());
        let disabled = after(&a, "--disable");
        for feature in [
            "shell_tool",
            "unified_exec",
            "view_image",
            "browser_use",
            "computer_use",
            "in_app_browser",
            "apps",
            "plugins",
        ] {
            assert!(
                disabled.contains(&feature),
                "{policy:?} must disable {feature}: {disabled:?}"
            );
        }
        let config = after(&a, "-c");
        assert!(
            config.contains(&format!("web_search=\"{search}\"").as_str()),
            "{config:?}"
        );
        assert!(config.contains(&"mcp_servers={}"));
        assert!(config.contains(&"mcp_servers.kb.url=\"http://127.0.0.1:9/kb/mcp\""));
        assert!(config.contains(&"mcp_servers.kb.bearer_token_env_var=\"KB_TOKEN\""));
        assert!(
            config.contains(&"mcp_servers.kb.default_tools_approval_mode=\"approve\""),
            "exec cannot ask, so the request's MCP tools are pre-approved: {config:?}"
        );
        assert!(config.contains(&"developer_instructions=\"Be brief.\""));
        assert!(config.contains(&"sandbox_mode=\"read-only\""), "{config:?}");
        let empty_reset = config.iter().position(|c| *c == "mcp_servers={}").unwrap();
        let kb = config
            .iter()
            .position(|c| c.starts_with("mcp_servers.kb.url"))
            .unwrap();
        assert!(
            empty_reset < kb,
            "user servers are dropped before ours are added"
        );
    }
}

#[tokio::test]
async fn devin_and_opencode_refuse_what_they_cannot_enforce() {
    let dir = tempfile::tempdir().unwrap();
    let devin =
        AgentCli::with_binary(Provider::Devin, recording_cli(dir.path(), "devin", "ok")).unwrap();
    for request in [
        RunRequest {
            tool_policy: ToolPolicy::Web,
            ..request(dir.path())
        },
        RunRequest {
            mcp_servers: vec![kb_server()],
            ..request(dir.path())
        },
    ] {
        assert!(matches!(
            devin.run(request).await,
            Err(Error::InvalidRequest(_))
        ));
    }
    assert!(!dir.path().join("args").exists(), "nothing was started");
}

#[tokio::test]
async fn devin_receives_the_system_prompt_before_the_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let body = format!(
        r#"prev=''; for a in "$@"; do [ "$prev" = --prompt-file ] && cp "$a" "{}"; prev="$a"; done; echo ok"#,
        dir.path().join("prompt").display()
    );
    let devin =
        AgentCli::with_binary(Provider::Devin, fake_cli(dir.path(), "devin", &body)).unwrap();
    devin
        .run(RunRequest {
            system_prompt: Some("Be brief.".into()),
            ..request(dir.path())
        })
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("prompt")).unwrap(),
        "<instructions>\nBe brief.\n</instructions>\n\nhello\nworld"
    );
}

#[tokio::test]
async fn invalid_mcp_servers_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let cli = AgentCli::with_binary(
        Provider::Claude,
        recording_cli(dir.path(), "claude", CLAUDE_OK),
    )
    .unwrap();
    let bad = [
        McpServer {
            name: "k b".into(),
            ..kb_server()
        },
        McpServer {
            url: "file:///etc/passwd".into(),
            ..kb_server()
        },
        McpServer {
            bearer_token_env: Some("A-B".into()),
            ..kb_server()
        },
    ];
    for server in bad {
        let req = RunRequest {
            mcp_servers: vec![server],
            ..request(dir.path())
        };
        assert!(matches!(cli.run(req).await, Err(Error::InvalidRequest(_))));
    }
    let dup = RunRequest {
        mcp_servers: vec![kb_server(), kb_server()],
        ..request(dir.path())
    };
    assert!(matches!(cli.run(dup).await, Err(Error::InvalidRequest(_))));
}

#[tokio::test]
async fn sessions_persist_and_resume() {
    let dir = tempfile::tempdir().unwrap();
    let claude = AgentCli::with_binary(
        Provider::Claude,
        recording_cli(dir.path(), "claude", CLAUDE_OK),
    )
    .unwrap();
    claude.run(request(dir.path())).await.unwrap();
    assert!(
        args(dir.path())
            .iter()
            .any(|a| a == "--no-session-persistence"),
        "ephemeral by default"
    );
    claude
        .run(RunRequest {
            persist_session: true,
            ..request(dir.path())
        })
        .await
        .unwrap();
    assert!(
        !args(dir.path())
            .iter()
            .any(|a| a == "--no-session-persistence")
    );
    claude
        .run(RunRequest {
            resume: Some("s-1".into()),
            ..request(dir.path())
        })
        .await
        .unwrap();
    assert_eq!(after(&args(dir.path()), "--resume"), ["s-1"]);

    let codex = AgentCli::with_binary(
        Provider::Codex,
        recording_cli(dir.path(), "codex", CODEX_OK),
    )
    .unwrap();
    codex
        .run(RunRequest {
            resume: Some("t-1".into()),
            ..request(dir.path())
        })
        .await
        .unwrap();
    let a = args(dir.path());
    assert_eq!(&a[..2], ["exec", "resume"]);
    assert_eq!(&a[a.len() - 2..], ["t-1", "-"]);
    assert!(
        !a.iter()
            .any(|x| x == "-s" || x == "--color" || x == "--ephemeral"),
        "{a:?}"
    );
    codex
        .run(RunRequest {
            persist_session: true,
            ..request(dir.path())
        })
        .await
        .unwrap();
    assert!(!args(dir.path()).iter().any(|x| x == "--ephemeral"));

    let devin =
        AgentCli::with_binary(Provider::Devin, recording_cli(dir.path(), "devin", "ok")).unwrap();
    let r = devin
        .run(RunRequest {
            resume: Some("x".into()),
            ..request(dir.path())
        })
        .await;
    assert!(matches!(r, Err(Error::InvalidRequest(_))));
}

#[tokio::test]
async fn missing_sessions_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    let claude = AgentCli::with_binary(
        Provider::Claude,
        fake_cli(
            dir.path(),
            "claude",
            "cat >/dev/null; echo 'No conversation found with session ID: s-9' >&2; exit 1",
        ),
    )
    .unwrap();
    let r = claude
        .run(RunRequest {
            resume: Some("s-9".into()),
            ..request(dir.path())
        })
        .await;
    assert!(
        matches!(r, Err(Error::SessionNotFound(ref id)) if id == "s-9"),
        "{r:?}"
    );

    let codex = AgentCli::with_binary(
        Provider::Codex,
        fake_cli(dir.path(), "codex", "cat >/dev/null; echo 'Error: thread/resume failed: no rollout found for thread id t-9' >&2; exit 1"),
    )
    .unwrap();
    let r = codex
        .run(RunRequest {
            resume: Some("t-9".into()),
            ..request(dir.path())
        })
        .await;
    assert!(matches!(r, Err(Error::SessionNotFound(_))), "{r:?}");
}

#[tokio::test]
async fn answers_quoting_session_errors_are_not_missing_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let codex = AgentCli::with_binary(
        Provider::Codex,
        recording_cli(
            dir.path(),
            "codex",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"no rollout found"}}"#,
        ),
    )
    .unwrap();
    let r = codex
        .run(RunRequest {
            resume: Some("t-9".into()),
            ..request(dir.path())
        })
        .await;
    assert!(r.is_ok(), "{r:?}");
}

#[tokio::test]
async fn session_ids_cannot_inject_flags() {
    let dir = tempfile::tempdir().unwrap();
    let codex = AgentCli::with_binary(
        Provider::Codex,
        fake_cli(dir.path(), "codex", "echo should-not-run; exit 9"),
    )
    .unwrap();
    for id in [
        "--dangerously-bypass-approvals-and-sandbox",
        "",
        "a b",
        "../x",
    ] {
        let r = codex
            .run(RunRequest {
                resume: Some(id.into()),
                ..request(dir.path())
            })
            .await;
        assert!(matches!(r, Err(Error::InvalidRequest(_))), "{id:?}: {r:?}");
    }
}

fn collect() -> (EventHandler, Arc<Mutex<Vec<AgentEvent>>>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    (
        EventHandler::new(move |e| sink.lock().unwrap().push(e)),
        events,
    )
}

#[tokio::test]
async fn claude_streams_events() {
    let dir = tempfile::tempdir().unwrap();
    let lines = [
        r#"{"type":"system","subtype":"init"}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"mcp__kb__kb_grep","input":{}}]}}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hel"}}}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"lo"}}}"#,
        r#"{"type":"result","is_error":false,"result":"Hello","session_id":"s-2","usage":{"input_tokens":1,"output_tokens":2}}"#,
    ];
    let body = format!(
        "cat >/dev/null\n{}",
        lines
            .iter()
            .map(|l| format!("printf '%s\\n' '{l}'"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let cli =
        AgentCli::with_binary(Provider::Claude, fake_cli(dir.path(), "claude", &body)).unwrap();
    let (handler, events) = collect();
    let result = cli
        .run(RunRequest {
            on_event: Some(handler),
            ..request(dir.path())
        })
        .await
        .unwrap();
    assert_eq!(result.text, "Hello");
    assert_eq!(result.session_id.as_deref(), Some("s-2"));
    assert_eq!(
        *events.lock().unwrap(),
        [
            AgentEvent::ToolUse {
                name: "mcp__kb__kb_grep".into()
            },
            AgentEvent::TextDelta("Hel".into()),
            AgentEvent::TextDelta("lo".into()),
        ]
    );
}

#[tokio::test]
async fn codex_reports_tools_and_messages() {
    let dir = tempfile::tempdir().unwrap();
    let body = r#"cat >/dev/null
printf '%s\n' '{"type":"thread.started","thread_id":"t-3"}' '{"type":"item.started","item":{"type":"web_search"}}' '{"type":"item.started","item":{"type":"mcp_tool_call","tool":"kb_get"}}' '{"type":"item.completed","item":{"type":"agent_message","text":"done"}}' '{"type":"turn.completed","usage":{}}'"#;
    let cli = AgentCli::with_binary(Provider::Codex, fake_cli(dir.path(), "codex", body)).unwrap();
    let (handler, events) = collect();
    let result = cli
        .run(RunRequest {
            on_event: Some(handler),
            ..request(dir.path())
        })
        .await
        .unwrap();
    assert_eq!(result.text, "done");
    assert_eq!(
        *events.lock().unwrap(),
        [
            AgentEvent::ToolUse {
                name: "web_search".into()
            },
            AgentEvent::ToolUse {
                name: "kb_get".into()
            },
            AgentEvent::Text("done".into()),
        ]
    );
}
