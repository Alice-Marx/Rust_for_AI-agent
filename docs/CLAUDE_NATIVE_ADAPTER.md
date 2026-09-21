# Claude Code native adapter

Wonderland invokes the installed official Claude Code native executable through
its `stream-json` protocol. It does not modify the upstream checkout and does not
substitute a direct API client. The local `claude-code-best` fork is a separate
manual terminal profile and is never accepted as this managed executor.

## Supported boundary

- The current verified protocol target is **Claude Code 2.1.193**, installed from
  `@anthropic-ai/claude-code` with a native `bin/claude[.exe]` entry. The adapter
  checks the package identity, exact version banner and executable SHA-256.
  These are local provenance records, not publisher-signature verification or
  cryptographic proof of the model serving the request. Standalone binaries,
  old JavaScript distributions and new versions remain available in a manual
  terminal until their protocol is validated.
- Authentication stays inside the official executable. Its normal subscription
  OAuth or Anthropic API account can be used. The adapter does not read credential
  files, import tokens, use `--bare`, or silently switch authentication providers.
  Initialization must report `firstParty`.
- A full `claude-*` model ID is mandatory. Model aliases, custom provider routes,
  fallback-model lists, refusal fallback, and subagents are disabled. Before the
  task is submitted, `get_settings` must report the requested applied model and
  requested effort. System initialization, every assistant message, and terminal
  model usage are checked again. An incompatible model or unknown capability
  fails the task; no direct API fallback is attempted.
- Chat exposes only **Read, Glob, Grep, AskUserQuestion**. Bash, write/edit tools,
  web fetches, plugins, skills, agents and MCP tools are excluded from its actual
  CLI tool set. This is a verified tool restriction, not an OS filesystem sandbox.
- Agent mode additionally exposes **Bash, Edit, Write, NotebookEdit**. Host-owned
  settings put those tools under explicit single-use approval. Question answers
  preserve the official question text and selection options. A missing or closed
  approval channel denies pending requests.
- Safe mode disables customizations. User/project/local settings sources are
  disabled; strict empty MCP configuration is checked before the prompt. All
  effective settings must equal the host restriction configuration. Nonempty
  managed policy is refused, not overridden. Organization policy still applies
  during official CLI startup; this integration is not an independent containment
  boundary for an enterprise-managed executable.

## Protocol and lifetime

Text and readable thinking deltas are forwarded live. Full assistant messages are
reconciled with streamed prefixes to avoid duplicate text. Hidden/redacted
thinking is not manufactured. The CLI owns its token caching, context and tool
execution. A new official UUID is recorded per run; `--no-session-persistence`
means this adapter does not advertise native transcript resume.

Cancellation and deadlines attempt the official interrupt request, close stdin,
and clean up the process tree with a bounded grace period. JSON frames, output,
message counts and pending permissions are bounded. EOF, error results, model
mismatch, unrecognized control requests, pending approvals at completion and
incomplete terminal reasons fail closed.

## Validation evidence

`tests/claude_native/protocol.rs` is included in the module's unit tests and covers
stream deduplication, model/setting boundaries, Chat tool restrictions, Agent
approval, question mapping, failed terminals and pre-prompt handshake rejection.

The opt-in command below probes the installed official CLI with an isolated
configuration directory, a dummy key, and a loopback endpoint that cannot reach
Anthropic. It submits no user prompt and reports only protocol capability fields:

```powershell
python tests/claude_native_probe.py D:/nodejs/npm-global/node_modules/@anthropic-ai/claude-code/bin/claude.exe
```

Adding `--mock-turn` sends a fixed fixture prompt to a local loopback SSE server.
It validates actual CLI thinking/text block ordering and terminal output without
contacting an AI provider. The normalized official trace is checked into
`tests/claude_native/official_2_1_193.jsonl` and replayed by the Rust unit tests.
Claude emits an assistant snapshot for each completed content block, reusing
the same message ID; the adapter therefore reconciles each block separately.

On the development machine, initialization, applied-settings and MCP-status
handshakes and a local mock turn succeeded against official **2.1.193**. No real Claude model request or
subscription account test has been performed for this adapter. Only Kimi was
authorized for live account testing in the current work session. Do not describe
Claude model execution as live-tested or performance-equivalent to the official
terminal on the basis of these protocol checks.
