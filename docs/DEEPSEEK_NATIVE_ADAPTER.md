# DeepSeek Harness native ACP adapter

The managed `deepseek` executor starts the unmodified official
`@deepseek-ai/dsh@0.1.6-alpha.2` CLI with `--profile acp --patch <managed file>`.
It does not replace Harness with a direct API call. Reference checkouts remain
unchanged and can still be opened in the manual terminal.

## Installation and credentials

Install the exact release with npm, then make `dsh` and `node` available on PATH.
An isolated installation can instead be selected with `WONDERLAND_DSH_CLI`, an
absolute path to `node_modules/@deepseek-ai/dsh/lib/bin.js`. Source checkouts,
standalone binaries, pnpm layouts, changed profile bundles and unverified releases
are rejected by the managed executor. They remain usable in the manual terminal.

The adapter checks the package name, repository metadata, exact release of every
installed `@deepseek-ai/dsh*` package in the npm scope, the CLI/version banner,
and SHA-256 fingerprints of the CLI entry and both official profile bundles.
The fingerprints document a tested release; they are not publisher-signature
verification or attestation of the remotely served model. Node and ordinary npm
dependencies remain part of the local trusted runtime.

Set `DEEPSEEK_API_KEY` in the environment that starts Wonderland. Only that
provider credential is inherited by the managed DeepSeek process. The adapter
does not read, display or copy the user's credential file. Each invocation uses
a new private Harness home and a neutral launch directory, so existing DSH
settings, executable patches, hooks and project `.env` routing cannot override
the selected provider. API-key login remains the DeepSeek authentication path;
the ACP release does not advertise a subscription OAuth authentication method.

## Protocol and constraints

The adapter verifies ACP protocol 1 and `deepseek-harness-acp` component version
`0.0.1` (different from the package release). It creates a fresh session, verifies
the selected `["deepseek-official", "<exact model ID>"]`, explicitly sets the
model through `session/set_config_option`, and checks the echo before prompting.
Optional reasoning effort uses DeepSeek's own `off`, `low`, `high`, or `max`
values; unsupported models/efforts fail rather than silently falling back.
Configuration notifications and session identities are checked throughout a run.

The official profile is patched through its supported public mechanism to disable
alternate provider routes, settings reload, plugin management, HMR, nested agents,
workflows, goal retries, auxiliary search-model calls and custom MCP servers.
Automatic title generation is disabled. The official DeepSeek messages endpoint
is explicit. File reads, searches, edits, shell tools, context compaction and
provider-specific prompt construction continue to belong to the official Harness.

Read-only sessions use the official read-only filesystem/shell policy with no
escalation approval and a single read-only permission preset. Writable sessions
use workspace-write with one-time approval for wider operations. There is no
persistent allow option. Missing tool attribution, a disconnected control UI,
replayed permission IDs and unsupported client methods never grant permission.

This is the upstream file sandbox, not complete machine isolation: upstream
documents partial Windows ACL enforcement and filesystem containment limitations.
The Rust process-tree guard provides process cleanup, not an additional security
sandbox. A keyless ACP handshake does not test actual tool confinement.

ACP emits committed assistant message/thought blocks rather than provider token
deltas. `usage_update` measures context occupancy, not billed input/output tokens
or price. Prompt `end_turn` means the official agent settled; it does not prove
that implementation or verification succeeded. Execution limits remain errors.
Questions, resume, fork, images and arbitrary ACP client filesystem/terminal
requests are not enabled in this adapter.

Every run bounds frames, cumulative output, pending approvals, request lifetime
and event delivery. Cancel, timeout, protocol failure and UI disconnection all
attempt `session/cancel` followed by `session/close`; the process tree is killed
after the bounded grace period. Fresh profile data is removed after shutdown.

## Evidence and tests

On 2026-09-21, the exact official npm package was installed into the isolated
toolchain directory `E:\harness\toolchains\wonderland-deepseek-probe` with npm
install scripts disabled. All 249 installed DSH-family packages had the exact
`0.1.6-alpha.2` version. The real CLI completed `--version`, ACP initialize,
session creation, exact model selection, reasoning selection and session close,
returning exit code 0. The restrictive managed profile also booted successfully.
No API key was inherited, no prompt was sent, and no paid DeepSeek request was made.

The Rust module includes offline fixtures for route pinning, session mismatch,
read-only denial, one-shot approval/replay, output limits, usage semantics and
cleanup ordering. Its ignored `official_keyless_initialize_configure_close` test
repeats the actual keyless handshake using the configured npm installation.
Run through the parent project's normal Cargo test process:

```powershell
cargo test --lib native_executor::deepseek
$env:WONDERLAND_DSH_CLI = 'E:\harness\toolchains\wonderland-deepseek-probe\node_modules\@deepseek-ai\dsh\lib\bin.js'
cargo test --lib native_executor::deepseek::tests::official_keyless_initialize_configure_close -- --ignored
```

Validation passed: Cargo compilation, all 13 offline Rust fixtures, the ignored
real `official_keyless_initialize_configure_close` integration test, rustfmt,
and the independent real CLI keyless probe. The integration test also exposed
and verified the fix for Node rejecting Windows `\\?\` entry-point paths; paths
are normalized only after checking they resolve to the same filesystem object.
Live inference, real tool execution and cancellation under paid model traffic
remain unverified.

Parent integration uses `deepseek::validate_request` and
`deepseek::execute_with_control`, with capability protocol `deepseek-acp` and
reasoning options `off/low/high/max`; `questions`, `resume` and `fork` are false.
