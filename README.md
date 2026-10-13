# agent-call

[![crates.io](https://img.shields.io/crates/v/agent-call.svg)](https://crates.io/crates/agent-call)
[![docs.rs](https://docs.rs/agent-call/badge.svg)](https://docs.rs/agent-call)
[![License: MIT OR Apache-2.0](https://img.shields.io/crates/l/agent-call.svg)](#license)

Call Claude, Codex, Devin, and OpenCode v2 from Rust through one async API.
`agent-call` uses your installed CLIs and their saved authentication. Each
request starts a fresh process and returns the final answer, elapsed time,
session ID, and token usage when the CLI reports it.

- Choose a provider and model without rewriting your application integration.
- Configure tool policies, HTTP MCP servers, and system prompts.
- Persist and resume sessions, and receive progress events where supported.
- Set timeouts and handle typed errors, including provider context.

[API reference](https://docs.rs/agent-call) · [crates.io](https://crates.io/crates/agent-call) · [Releases](https://github.com/tidusvn05/agent-call/releases)

## Requirements

Install and authenticate the CLI you want to use. `AgentCli::discover` finds
it on `PATH`. OpenCode must report major version 2; version 1 is rejected.
No provider API key is required by the library itself. Use `env_remove` if your
application needs to remove environment variables before spawning the CLI.

## Quick start

Add the published crate:

```toml
[dependencies]
agent-call = "0.4.1"
tokio = { version = "1", features = ["macros", "rt"] }
```

Then run a prompt:

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

Set `model: None` to use the selected CLI's configured default.

## Providers

| CLI | Rust provider | Example model | JSON Schema | Sessions | Progress events |
|---|---|---|---|---|---|
| Claude | `Provider::Claude` | `sonnet@high` | Native | Persist and resume | Text deltas and tools |
| Codex | `Provider::Codex` | `gpt-5.6-sol@high` | Native | Persist and resume | Messages and tools |
| Devin | `Provider::Devin` | `swe-2-medium` | Caller validates | Unsupported | Unsupported |
| OpenCode v2 | `Provider::OpenCodeV2` | `openai/gpt-5#high` | Caller validates | CLI persists; resume by ID | Messages and tools |

Choose model IDs available in your authenticated CLI configuration. The
[runnable example](examples/run.rs) accepts a provider and an optional model:

```sh
cargo run --example run -- claude
cargo run --example run -- codex
cargo run --example run -- devin
cargo run --example run -- opencode
cargo run --example run -- claude sonnet@high
```

Run these commands from a clone of this repository.

## Tools, MCP servers and system prompts

The default policy uses each CLI's permissions described under
[Execution behavior](#execution-behavior). For untrusted prompts, choose
`ToolPolicy::McpOnly` or `ToolPolicy::Web` to restrict built-in tools. These
policies disable shell, file access, and code execution in the CLI. Configure
MCP servers to expose only the tools your application intends to allow.

| `tool_policy` | Agent may use | Claude | Codex | Devin, OpenCode |
|---|---|---|---|---|
| `Default` | CLI permissions described below | ✅ | ✅ | ✅ |
| `McpOnly` | tools of the request's `mcp_servers` | ✅ | ✅ | `InvalidRequest` |
| `Web` | `McpOnly` plus built-in web search and fetch | ✅ | ✅ | `InvalidRequest` |

```rust
use agent_call::{McpServer, RunRequest, ToolPolicy};

let request = RunRequest {
    prompt: "Find the setup guide in our knowledge base.".into(),
    cwd: std::env::current_dir()?,
    tool_policy: ToolPolicy::Web,
    mcp_servers: vec![McpServer {
        name: "kb".into(),
        url: "http://127.0.0.1:8080/mcp".into(),
        bearer_token_env: Some("KB_TOKEN".into()),
    }],
    env: vec![("KB_TOKEN".into(), std::env::var("KB_TOKEN")?.into())],
    system_prompt: Some("You answer questions about our products.".into()),
    ..RunRequest::default()
};
```

Claude and Codex support per-request HTTP MCP servers. With a restricted
policy, the request's servers replace servers from the CLI's configuration.
Bearer tokens are passed through the child environment. A provider that cannot
enforce a requested policy or MCP configuration returns `Error::InvalidRequest`
before starting a process.

`system_prompt` appends instructions to the CLI's own system prompt for Claude
and Codex. Devin and OpenCode receive those instructions before the user prompt.
Use `AgentCli::native_system_prompt()` to check support. See [SPEC.md](SPEC.md)
for the native command and permission contracts.

## Sessions and progress events

Claude and Codex runs are ephemeral by default. Set `persist_session: true`
to retain a session, then pass `RunResult::session_id` as `resume` in a later
request with the same `cwd`. OpenCode manages persistence itself; Devin does
not support session resumption.

| Provider | Persist | Resume | Unknown session |
|---|---|---|---|
| Claude | drops `--no-session-persistence` | `--resume <id>` | `Error::SessionNotFound` |
| Codex | drops `--ephemeral` | `codex exec resume <id>` (sandbox via `-c sandbox_mode`) | `Error::SessionNotFound` |
| OpenCode v2 | always | `--session <id>` | the CLI starts a new session with that id |
| Devin | — | `InvalidRequest` | — |

`on_event: Some(EventHandler::new(|event| …))` reports progress while the run goes on:
`AgentEvent::TextDelta` (Claude streams the answer with `--output-format stream-json`),
`AgentEvent::Text` (whole messages from Codex and OpenCode) and `AgentEvent::ToolUse { name }`
(web searches, MCP tools). The final `RunResult` is the same as without events.

## Structured output and errors

Set `RunRequest::json_schema` to request native JSON Schema enforcement from
Claude or Codex. Devin and OpenCode return text for your application to validate.
`RunResult::usage` is `None` when token usage is unavailable or incomplete.

Use `discover_with_context`, `with_binary_with_context`, and
`run_with_context` when the caller needs the selected provider alongside a
failure. `ProviderError` exposes `provider` and the original `source: Error`;
`exit_code()` and `stderr_tail()` read the fields of an `Error::Exit` without
parsing its display text. The original methods remain available.

## Development

Run the automated tests with fake CLIs, without using provider quota:

```sh
cargo fmt --check
cargo test --locked
```

### Real CLI smoke tests

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

## Execution behavior

Each request starts a new CLI process. A timeout or cancellation terminates
that process; the library does not retry automatically. Claude uses
`bypassPermissions`, Codex uses a read-only sandbox, Devin uses its `auto`
permission mode, and OpenCode follows its headless permission defaults without
`--auto`.

## Releases

The Rust crate is published on [crates.io](https://crates.io/crates/agent-call).
GitHub Releases provide version tags and changelogs. GitHub's Packages sidebar
is separate from crates.io and does not list this crate.

For maintainers: update `Cargo.toml`, `Cargo.lock`, and the dependency example
above, commit the changes, then test and publish from a clean working tree:

```sh
cargo test --locked
cargo publish --dry-run --locked
cargo publish --locked
version=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version')
git tag -a "v$version" -m "agent-call v$version"
git push origin HEAD:main "v$version"
```

The [release workflow](.github/workflows/release.yml) tests and packages the
tagged crate, then creates a GitHub Release with a changelog of commits since
the previous tag. It does not publish to crates.io. To create a GitHub Release
for an existing tag, run the workflow manually with that tag. Re-running it
for the same tag is safe.

## License

Licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
