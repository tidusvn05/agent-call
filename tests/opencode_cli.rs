#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_core::{Error, OpenCodeV2, RunRequest, TokenUsage};

fn fake_cli(dir: &Path, body: &str) -> std::path::PathBuf {
    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
    let path = dir.join(format!(
        "opencode-{}",
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(
        &path,
        format!("#!/bin/sh\nif [ \"$1\" = --version ]; then echo 2.0.7; exit 0; fi\n{body}\n"),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

fn request(dir: &Path) -> RunRequest {
    RunRequest {
        prompt: "large\nprompt".into(),
        cwd: dir.into(),
        model: Some("openai/model#high".into()),
        timeout: Duration::from_secs(2),
        env_remove: vec!["TEST_API_KEY".into()],
        json_schema: None,
        ..RunRequest::default()
    }
}

#[tokio::test]
async fn parses_completed_text_and_usage() {
    let dir = tempfile::tempdir().unwrap();
    let binary = fake_cli(
        dir.path(),
        r#"
[ "$1" = run ] && [ "$2" = --standalone ] && [ "$3" = --format ] && [ "$4" = json ] || exit 8
[ "$5" = --model ] && [ "$6" = 'openai/model#high' ] || exit 9
[ -z "${TEST_API_KEY+x}" ] || exit 10
test "$(cat)" = 'large
prompt' || exit 11
printf '%s\n' \
  '{"type":"step_start","sessionID":"ses_1"}' \
  '{"type":"text","sessionID":"ses_1","part":{"text":"Hello"}}' \
  '{"type":"text","sessionID":"ses_1","part":{"text":"world"}}' \
  '{"type":"step_finish","sessionID":"ses_1","part":{"reason":"stop","tokens":{"input":2,"output":3,"reasoning":1,"cache":{"read":4,"write":5}}}}'
"#,
    );
    let backend = OpenCodeV2::with_binary(binary).unwrap();
    let result = backend.run(request(dir.path())).await.unwrap();
    assert_eq!(result.text, "Hello\nworld");
    assert_eq!(result.session_id.as_deref(), Some("ses_1"));
    assert_eq!(
        result.usage,
        Some(TokenUsage {
            input: 2,
            output: 3,
            reasoning: 1,
            cache_read: 4,
            cache_write: 5
        })
    );
}

#[tokio::test]
async fn errors_and_incomplete_usage() {
    let dir = tempfile::tempdir().unwrap();
    let binary = fake_cli(
        dir.path(),
        "cat >/dev/null\nprintf '%s\\n' '{\"type\":\"error\",\"error\":{\"data\":{\"message\":\"bad model\"}}}'",
    );
    let backend = OpenCodeV2::with_binary(binary).unwrap();
    assert!(
        matches!(backend.run(request(dir.path())).await, Err(Error::Agent(s)) if s == "bad model")
    );

    let binary = fake_cli(
        dir.path(),
        "cat >/dev/null\nprintf '%s\\n' '{\"type\":\"step_start\"}' '{\"type\":\"text\",\"part\":{\"text\":\"ok\"}}'",
    );
    let backend = OpenCodeV2::with_binary(binary).unwrap();
    assert!(
        backend
            .run(request(dir.path()))
            .await
            .unwrap()
            .usage
            .is_none()
    );
}

#[tokio::test]
async fn rejects_bad_stream_and_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let binary = fake_cli(dir.path(), "cat >/dev/null\necho not-json");
    let backend = OpenCodeV2::with_binary(binary).unwrap();
    assert!(matches!(
        backend.run(request(dir.path())).await,
        Err(Error::Protocol(_))
    ));

    let binary = fake_cli(dir.path(), "cat >/dev/null\nexec sleep 5");
    let backend = OpenCodeV2::with_binary(binary).unwrap();
    let mut req = request(dir.path());
    req.timeout = Duration::from_millis(50);
    assert!(matches!(backend.run(req).await, Err(Error::Timeout(_))));
}

#[test]
fn requires_v2() {
    let dir = tempfile::tempdir().unwrap();
    let binary = fake_cli(dir.path(), "exit 0");
    std::fs::write(&binary, "#!/bin/sh\necho 1.18.0\n").unwrap();
    assert!(matches!(
        OpenCodeV2::with_binary(binary),
        Err(Error::UnsupportedVersion(_))
    ));
}

#[tokio::test]
async fn refuses_tool_policies_and_mcp() {
    let dir = tempfile::tempdir().unwrap();
    let cli = OpenCodeV2::with_binary(fake_cli(dir.path(), "echo should-not-run; exit 9")).unwrap();
    let req = RunRequest {
        tool_policy: agent_core::ToolPolicy::McpOnly,
        ..request(dir.path())
    };
    assert!(matches!(cli.run(req).await, Err(Error::InvalidRequest(_))));
}
