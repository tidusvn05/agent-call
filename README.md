# agent-call

`agent-call` is a Rust library for one-shot calls to four local agent CLIs:
Claude, Codex, Devin, and OpenCode v2. It uses each CLI's saved authentication
and starts a fresh process for every request. The caller supplies a prompt and
gets final text, elapsed time, and token usage when the CLI reports it.

## Requirements

Install and authenticate the CLI you want to use. `AgentCli::discover` finds
it on `PATH`. OpenCode must report major version 2; version 1 is rejected.
No provider API key is required by the library itself. Use `env_remove` if your
application needs to remove environment variables before spawning the CLI.

## Add the dependency

```toml
[dependencies]
agent-call = "0.4.1"
tokio = { version = "1", features = ["macros", "rt"] }
```

## Run a prompt

```rust
use agent_call::{AgentCli, Provider, RunRequest};
use std::time::Duration;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let agent = AgentCli::discover(Provider::Claude)?;
    let result = agent.run(RunRequest {
        prompt: "Summarize this repository in three sentences.".into(),
        cwd: std::env::current_dir()?,
        model: None, // use the CLI's configured default
        timeout: Duration::from_secs(300),
        ..RunRequest::default()
    }).await?;

    println!("{}", result.text);
    Ok(())
}
```

Change `Provider::Claude` to select another CLI. The repository also includes
a runnable [four-provider example](examples/run.rs):

```sh
cargo run --example run -- claude
cargo run --example run -- codex
cargo run --example run -- devin
cargo run --example run -- opencode
```

Run those commands from this repository after authenticating the selected CLI.
The example accepts an optional model as a second argument.

## Provider examples

The following calls use the same `RunRequest` shape shown above. Set `model`
to `None` to inherit the selected CLI's configured default.

| Provider | Rust selection | Example `model` | Native command |
|---|---|---|---|
| Claude | `Provider::Claude` | `Some("sonnet@high".into())` | `claude -p --output-format json` |
| Codex | `Provider::Codex` | `Some("gpt-5.6-sol@high".into())` | `codex exec --json` |
| Devin | `Provider::Devin` | `Some("swe-2-medium".into())` | `devin -p --prompt-file <file>` |
| OpenCode v2 | `Provider::OpenCodeV2` | `Some("openai/gpt-5#high".into())` | `opencode run --standalone --format json` |

To try an explicit model for each provider:

```sh
cargo run --example run -- claude sonnet@high
cargo run --example run -- codex gpt-5.6-sol@high
cargo run --example run -- devin swe-2-medium
cargo run --example run -- opencode openai/gpt-5#high
```

Choose model IDs available in your authenticated CLI configuration.

## Tools, MCP servers and system prompts

By default each CLI keeps its historical permissions (see [Execution behavior](#execution-behavior)),
which suits trusted batch jobs. For untrusted input, such as a chat bot forwarding user messages,
choose a restricted `tool_policy`. No restricted policy gives the agent a shell, file access or code
execution.

| `tool_policy` | Agent may use | Claude | Codex | Devin, OpenCode |
|---|---|---|---|---|
| `Default` | everything the CLI allowed before 0.4 | ✅ | ✅ | ✅ |
| `McpOnly` | tools of the request's `mcp_servers` | ✅ | ✅ | `InvalidRequest` |
| `Web` | `McpOnly` plus built-in web search and fetch | ✅ | ✅ | `InvalidRequest` |

```rust
use agent_call::{McpServer, RunRequest, ToolPolicy};

let request = RunRequest {
    prompt: user_message,
    cwd: empty_work_dir,
    tool_policy: ToolPolicy::Web,
    mcp_servers: vec![McpServer {
        name: "kb".into(),
        url: "http://127.0.0.1:8080/mcp".into(),
        bearer_token_env: Some("KB_TOKEN".into()),
    }],
    env: vec![("KB_TOKEN".into(), run_token.into())], // never on the command line
    system_prompt: Some("You answer questions about our products.".into()),
    ..RunRequest::default()
};
```

- **Claude** runs restricted policies with `--tools ""` (or `WebSearch,WebFetch`),
  `--allowedTools mcp__<name>…`, `--permission-mode dontAsk`, `--strict-mcp-config` and
  `--disable-slash-commands`, and with `CLAUDE_CODE_DISABLE_ATTACHMENTS=1` so `@path` mentions in
  the prompt are not inlined as file contents; MCP servers are passed with `--mcp-config` and the
  token header reads `${VAR}` from the child environment.
- **Codex** runs restricted policies with its shell, exec, image, browser, computer-use, apps and
  plugins features disabled, the read-only sandbox, `web_search` set to `live` or `disabled`, and the user's
  own `mcp_servers` replaced by the request's (`bearer_token_env_var`).
- **System prompts** are appended to the CLI's own (Claude `--append-system-prompt`, Codex
  `developer_instructions`); Devin and OpenCode receive them before the prompt.
  `AgentCli::native_system_prompt()` tells which case applies.
- A CLI that cannot enforce a policy or MCP servers returns `Error::InvalidRequest` and starts nothing.

The opt-in tests `claude_restricted_runs_cannot_read_files` and `codex_restricted_runs_cannot_read_files`
check with the real CLIs that a restricted run cannot read a file in its working directory, with a
tool or through an `@path` mention.

## Sessions and progress events

Runs are ephemeral unless asked otherwise. Set `persist_session: true` to keep the CLI's session,
then pass `RunResult::session_id` as `resume` in a later request with the same `cwd`:

| | Persist | Resume | Unknown session |
|---|---|---|---|
| Claude | drops `--no-session-persistence` | `--resume <id>` | `Error::SessionNotFound` |
| Codex | drops `--ephemeral` | `codex exec resume <id>` (sandbox via `-c sandbox_mode`) | `Error::SessionNotFound` |
| OpenCode v2 | always | `--session <id>` | the CLI starts a new session with that id |
| Devin | — | `InvalidRequest` | — |

`on_event: Some(EventHandler::new(|event| …))` reports progress while the run goes on:
`AgentEvent::TextDelta` (Claude streams the answer with `--output-format stream-json`),
`AgentEvent::Text` (whole messages from Codex and OpenCode) and `AgentEvent::ToolUse { name }`
(web searches, MCP tools). The final `RunResult` is the same as without events.

## Structured errors

Use `discover_with_context`, `with_binary_with_context`, and
`run_with_context` when the caller needs the selected provider alongside a
failure. `ProviderError` exposes `provider` and the original `source: Error`;
`exit_code()` and `stderr_tail()` read the fields of an `Error::Exit` without
parsing its display text. The original methods remain available.

## Real CLI smoke tests

The opt-in smoke tests make one short request per provider. Set the model to
one supported by your authenticated CLI, then run a single test:

```sh
AGENT_CALL_CODEX_MODEL='gpt-5.6-sol@high' \
  cargo test --test real_cli_smoke codex -- --ignored
```

Replace `CODEX` and `codex` with `CLAUDE`/`claude`, `DEVIN`/`devin`, or
`OPENCODE`/`opencode`. These calls use real quota and have a 180-second timeout.
Claude and Codex receive a native JSON Schema; Devin and OpenCode output is
checked as JSON by the test because their adapters do not enforce schemas.
The regular `cargo test` suite uses fake CLIs and does not make real calls.

## Releases

Release tags use `vMAJOR.MINOR.PATCH` and must match the version in
`Cargo.toml`. After updating `Cargo.toml`, `Cargo.lock`, and the README's
dependency example, publish to crates.io, then push the commit and its tag:

```sh
cargo publish --locked
git tag -a v0.4.1 -m 'agent-call v0.4.1'
git push origin main v0.4.1
```

The [release workflow](.github/workflows/release.yml) tests and packages the
tagged crate, then creates a GitHub Release. Its body and attached
`CHANGELOG.md` list every commit since the previous version tag, including
commits made outside pull requests. To create a GitHub Release for an existing
tag, run the Release workflow manually and enter the tag. Re-running the
workflow for the same tag is safe.
The workflow creates GitHub Releases; crates.io publishing uses `cargo publish`.

Claude and Codex accept an optional `json_schema` and enforce it through their
native CLI flags. Devin and OpenCode return text for the caller to validate.
`RunResult::usage` is `None` when usage is unavailable or incomplete. For the
exact command and output contracts, see [SPEC.md](SPEC.md).

## Execution behavior

Each request starts a new CLI process. A timeout or cancellation terminates
that process; the library does not retry automatically. Claude uses
`bypassPermissions`, Codex uses a read-only sandbox, Devin uses its `auto`
permission mode, and OpenCode follows its headless permission defaults without
`--auto`.

License: [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE).
