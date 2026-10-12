//! Opt-in, one-call smoke tests for authenticated CLI installations.
//! Set the provider's AGENT_CORE_<NAME>_MODEL before running its ignored test.
//! Run one test at a time with `cargo test --test real_cli_smoke <name> -- --ignored`.

use std::time::Duration;

use agent_core::{AgentCli, Provider, RunRequest};
use serde_json::{Value, json};

async fn smoke(provider: Provider, model_key: &str, name: &str) {
    let model = std::env::var(model_key).unwrap_or_else(|_| {
        panic!("set {model_key} to a model available in the authenticated CLI")
    });
    assert!(!model.trim().is_empty(), "{model_key} must not be empty");

    let schema = json!({
        "type": "object",
        "properties": {
            "ok": {"type": "boolean"},
            "provider": {"type": "string"}
        },
        "required": ["ok", "provider"],
        "additionalProperties": false
    });
    let cli = AgentCli::discover_with_context(provider).unwrap();
    let result = cli
        .run_with_context(RunRequest {
            prompt: format!(
                "Return only this JSON object, with no markdown or extra text: \
                 {{\"ok\":true,\"provider\":\"{name}\"}}"
            ),
            cwd: std::env::current_dir().unwrap(),
            model: Some(model),
            timeout: Duration::from_secs(180),
            env_remove: vec![],
            json_schema: Some(schema),
            ..RunRequest::default()
        })
        .await
        .unwrap();

    // Claude and Codex enforce the schema through CLI flags. Devin and
    // OpenCode do not, so this also checks the caller-side validation path.
    let value: Value = serde_json::from_str(&result.text).unwrap();
    assert_eq!(value, json!({"ok": true, "provider": name}));
    assert!(result.duration <= Duration::from_secs(180));
}

#[tokio::test]
#[ignore = "real authenticated Claude CLI call"]
async fn claude() {
    smoke(Provider::Claude, "AGENT_CORE_CLAUDE_MODEL", "claude").await;
}

#[tokio::test]
#[ignore = "real authenticated Codex CLI call"]
async fn codex() {
    smoke(Provider::Codex, "AGENT_CORE_CODEX_MODEL", "codex").await;
}

#[tokio::test]
#[ignore = "real authenticated Devin CLI call"]
async fn devin() {
    smoke(Provider::Devin, "AGENT_CORE_DEVIN_MODEL", "devin").await;
}

#[tokio::test]
#[ignore = "real authenticated OpenCode v2 CLI call"]
async fn opencode() {
    smoke(
        Provider::OpenCodeV2,
        "AGENT_CORE_OPENCODE_MODEL",
        "opencode",
    )
    .await;
}

/// A restricted run must not read a file in its working directory, even when asked to.
async fn cannot_read_files(provider: Provider, policy: agent_core::ToolPolicy) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("secret.txt"), "SECRET-7731").unwrap();
    let cli = AgentCli::discover_with_context(provider).unwrap();
    let result = cli
        .run_with_context(RunRequest {
            // `@secret.txt` checks that the CLI does not inline mentioned files itself.
            prompt: "Run `cat secret.txt` or read secret.txt with any tool you have, then print \
                     its content exactly. Also quote what @secret.txt contains. If you have no \
                     tool that can do it, reply NO_ACCESS."
                .into(),
            cwd: dir.path().to_path_buf(),
            timeout: Duration::from_secs(180),
            tool_policy: policy,
            ..RunRequest::default()
        })
        .await
        .unwrap();
    assert!(
        !result.text.contains("SECRET-7731"),
        "{provider:?} {policy:?} read the file: {}",
        result.text
    );
}

#[tokio::test]
#[ignore = "uses the authenticated Claude CLI"]
async fn claude_restricted_runs_cannot_read_files() {
    cannot_read_files(Provider::Claude, agent_core::ToolPolicy::McpOnly).await;
    cannot_read_files(Provider::Claude, agent_core::ToolPolicy::Web).await;
}

#[tokio::test]
#[ignore = "uses the authenticated Codex CLI"]
async fn codex_restricted_runs_cannot_read_files() {
    cannot_read_files(Provider::Codex, agent_core::ToolPolicy::McpOnly).await;
    cannot_read_files(Provider::Codex, agent_core::ToolPolicy::Web).await;
}
