# agent-call 0.4 specification

`agent-call` is a Rust library for one-shot calls to four installed agent CLIs.
It does not start or manage a persistent chat session. Authentication is owned
by each CLI. A caller supplies a prompt, working directory, optional model,
timeout, optional JSON Schema, and names of child environment variables to
remove. Every invocation starts a fresh process and returns final text,
optional session ID and token usage, elapsed time, and the stderr tail.

| Provider | Native command | Prompt | Model format | Output | JSON Schema |
|---|---|---|---|---|---|
| Claude | `claude -p --output-format json` | stdin | `sonnet@high` or CLI default | JSON envelope | `--json-schema` |
| Codex | `codex exec --json -o <file> -` | stdin | `gpt-5.6-sol@high` or CLI default | JSONL + final-message file | `--output-schema <file>` |
| Devin | `devin -p --prompt-file <file>` | temporary file | CLI model ID or default | stdout | unsupported |
| OpenCode v2 | `opencode run --standalone --format json` | stdin | `provider/model#variant` or CLI default | JSONL | unsupported |

Only OpenCode major version 2 is accepted. `AgentCli::discover` and
`AgentCli::with_binary` check `opencode --version` before a run. The other
adapters do not impose a major-version constraint.

Claude and Codex return structured output errors even if their process exits
successfully. Codex uses the final-message file when present and the last
`agent_message` event otherwise. OpenCode token usage is absent when its
stream does not report a completed final step. Devin currently exposes no
token usage through this interface.

Timeout and future cancellation terminate the child process. The library
never retries automatically. OpenCode uses its headless permission defaults;
the adapter does not pass `--auto`. Claude, Codex, and Devin retain the
permission settings from their earlier agentwiki adapters.

Since 0.4 a request also carries a `ToolPolicy` (`Default`, `McpOnly`, `Web`), HTTP
`McpServer`s, extra child environment variables and an optional system prompt. `Default` keeps the
permissions above. `McpOnly` and `Web` are enforced by Claude (`--tools`, `--allowedTools`,
`dontAsk`, `--strict-mcp-config`, `--disable-slash-commands`, `CLAUDE_CODE_DISABLE_ATTACHMENTS=1`
against `@path` file inlining) and Codex (shell, exec, image, browser, computer-use, apps and
plugins features disabled, read-only sandbox, `web_search` live or disabled, user MCP servers replaced);
Devin and OpenCode reject them and per-run MCP servers with `InvalidRequest` before starting.

Requests may persist and resume sessions (`persist_session`, `resume`; Claude, Codex and
OpenCode) and report progress through `on_event` (`AgentEvent::TextDelta`, `Text`, `ToolUse`).
Resuming an unknown Claude or Codex session fails with `SessionNotFound`.

The public API is `Provider`, `AgentCli`, `RunRequest`, `RunResult`,
`TokenUsage`, `ToolPolicy`, `McpServer`, `AgentEvent`, `EventHandler`, `Error`, and
`ProviderError`. `AgentCli` also has context-preserving
discovery and run methods that attach the selected provider to the original
error. `OpenCodeV2` remains available for direct v2 calls.
