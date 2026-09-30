# climemory — project memory for coding agents

climemory (`cm`) is a Rust CLI that retrieves project requirements,
decisions and session history for a coding agent. It combines user-owned reference documents
with advisory memory, returns evidence with source addresses, and imports new
Codex session events into persistent topic threads.

The current package version is **2.62.1**. The public interface is a read-only
memory chat, a native MCP `ask` tool, explicit session ingestion, and optional
Codex hooks. Chat does not edit source documents or memory facts; ingestion is
the separate write path. Use ordinary development tools to search source code.

| Name | Role |
| --- | --- |
| `climemory` | Public GitHub repository and Rust package. |
| `@gangelxxx/climemory` | npm installer package. |
| `cm.exe` / `cm` | Executable installed in your project. |

Format identifiers, managed integration markers and installer state use
`climemory`. The executable stays `cm`.

[Installation](#quick-start) · [Commands](#commands) ·
[Configuration](#configuration-and-providers) · [Saving memory](#saving-session-memory) ·
[MCP and hooks](#mcp-and-automatic-hooks) · [Releases](#publishing-and-updating-releases)

## Quick start

The npm installation path requires a published `@gangelxxx/climemory` release. Maintainers
setting it up for the first time should follow [first publication](#first-publication).

With **Node.js 22.14+ and npm**, run this in the project where you want memory:

```powershell
cd path/to/your-project
npx @gangelxxx/climemory@latest init
```

The installer downloads the binary for its exact version from GitHub Releases,
verifies its SHA-256 and size, and saves `cm.exe` / `cm` in the current directory.
It then initializes memory and MCP using that permanent path. Your project does
not need Rust, Node.js source code, or its own `package.json`.

Next, configure model/provider access in `memory/config.json`, put reference
documents in `memory/docs/`, and check the connection. In PowerShell:

```powershell
.\cm.exe -test_providers -pretty
.\cm.exe "Find the requirements for the Save button, including exceptions"
```

On Linux or macOS:

```sh
./cm -test_providers -pretty
./cm "Find the requirements for the Save button, including exceptions"
```

Open or restart Codex in this project and trust its project settings when
prompted. The configured MCP tool can then retrieve memory for the coding agent.
Provider tests make real model calls and can consume tokens.

The npm wrapper supports `init`, `--help`, and `--version`. After installation,
run memory commands through `cm.exe` / `cm`; installation does not add the
executable to your global `PATH`.

### Supported platforms

| Platform | Release binaries |
| --- | --- |
| Windows | x64 |
| Linux with glibc | x64 and ARM64; built on Ubuntu 22.04 |
| macOS | Intel and Apple Silicon; built/tested by the workflow on macOS 15 |

Linux musl/Alpine and native Windows ARM64 need a source build. Release binaries
are not code-signed or notarized by this workflow.

### Update an installation

From the same project directory:

```powershell
npx @gangelxxx/climemory@latest init
```

Existing memory, documents and configuration are preserved. On Windows, close
running CM/MCP processes before replacing the executable. The installer refuses
to overwrite an unrelated or modified `cm` binary; move it aside explicitly if
you intend to replace it. Installation receipts live in `.climemory-install/`,
which the installer adds to `.gitignore`.

### Build from source

From a checkout of this repository, use a current stable Rust toolchain and your
platform's native linker/build tools:

```powershell
cargo build --locked --release --bin cm
```

The executable is `target/release/cm.exe` on Windows or `target/release/cm` on
Unix. Copy it to a permanent location before initialization: MCP records the
executable's path. Run it from the target project with `init`.

If you put the executable on `PATH`, run these commands from the project whose
memory you want to maintain; otherwise use its full or project-local path:

```powershell
cm init
# Configure model access in memory/config.json; add references to memory/docs.
cm -test_providers -pretty
```

`cm init` creates missing memory files, updates the managed CM block in
`AGENTS.md`, adds runtime/binary exclusions to `.gitignore`, and configures
`mcp_servers.cm` in the project's `.codex/config.toml`.
It preserves existing notes, reference documents and unrelated settings. An
existing unmanaged MCP server named `cm` causes an error instead of being
overwritten. Open or restart Codex in that project and trust its settings when
prompted. Initialization does not install automatic history hooks.

The generated agent profiles currently use the Codex adapter with model
`gpt-5.5` and low/medium/high reasoning effort. These are configuration defaults,
not a guarantee of account access: select models and credentials available in
your environment before making requests. Provider tests make real model calls.

CM resolves the nearest initialized project above the current directory, falling
back to initialized memory next to its executable. Run from the intended project
directory; the public chat has no project-selection flag.

## Commands

The reference below uses `cm` as shorthand. For an npm-installed project binary,
use `.\cm.exe` in PowerShell or `./cm` on Unix, unless you have added it to `PATH`.

| Command | Purpose |
| --- | --- |
| `cm "<question>"` | Retrieve memory in one turn and exit. Quote the entire message. |
| `cm` | Read questions interactively or from stdin; EOF ends input. |
| `cm "@context:ID <follow-up>"` | Continue the returned `context_session`. |
| `cm "@context:ID @details"` | Expand saved evidence and interpretations for that topic. |
| `cm init` | Initialize memory and project MCP/instructions. |
| `cm help` | Show the public CLI help and version. |
| `cm ingest-session` | Import new events from the identified Codex session. |
| `cm -test_providers` | Probe configured model profiles without sending memory. |
| `cm feedback "<description>"` | Save feedback without a model call or requirement changes. |
| `cm hooks install codex` | Install automatic history/context hooks for this project. |
| `cm hooks uninstall codex` | Remove CM handlers while preserving unrelated hooks. |
| `cm hooks status` | Show the number of queued sessions and hook runtime directory. |
| `cm hooks drain` | Process queued imports in the foreground. |
| `cm --mcp` | Run the native stdio MCP server, normally launched by the client. |

A question accepts 1–8000 characters. New questions start independent topics,
including in interactive mode; use `@context:ID` to preserve topic context.
There are no public `context`, `ask`, `reply`, `report`, or `code` subcommands.
MCP's `ask` is a tool name, not a CLI subcommand.

For chat and ordinary CLI commands, add `-pretty` (alias `--pretty`) for readable
terminal output. Add one of `-en`, `-ru`, or `-zh` for English, Russian, or
Simplified Chinese; language flags require `-pretty`. English is the default.
Source quotations and JSON keys are not translated. Hook and MCP commands use
their own machine protocols; do not add presentation flags to them.

```powershell
cm "What requirements are still unresolved?" -pretty -ru
cm ingest-session -pretty -ru
cm -test_providers -pretty
```

## Documents, retrieval and answers

Put reference material under `memory/docs/`, including nested directories. Files
must be UTF-8 text; Markdown and plain text work directly. Binary PDF/Office
documents need conversion first. The loader rejects symlinks, non-text control
characters, more than 1024 filesystem entries, or more than 4,000,000 bytes in
total instead of silently skipping material.

CM builds a local index of documents, thread notes and current imported claims.
Document sections become derived threads without changing the originals. Local
search selects matching roots and bounded child summaries (passports); recursive
agents consult relevant branches and combine their evidence. A verification
agent checks coverage against reviewed originals. This verifies an answer's
support in those sources; it does not run tests or prove the implementation works.

User documents outrank advisory memory. A newer timestamp does not make a memory
claim more authoritative. Conflicts, missing aspects and failed retrieval work
keep an answer partial; evidence from successful workers remains usable. A
missing result does not prove that a requirement does not exist. There is no
fallback to the retired coordinator pipeline.

Read `status`, gaps, conflicts and limitations, not just the process exit code.
A partial answer can return exit code zero. After a nonzero exit, inspect stdout
for any usable partial evidence as well as stderr for the error.

Responses can contain per-aspect answers, original source context, or compact
evidence instead of a single `answer` string. Consumers should handle:

- `format: "cm/compact-1"`: `columns` defines the cells in each `rows` array;
  `evidence_defaults` applies only to current evidence rows. Omitted compact
  metadata defaults to summary/full response, found aspects, no conflicts and
  no reused evidence. Source blocks default to user-document/source-context.
- `source_ref`: resolves in the current `sources` map. `question_ref` refers to
  an earlier question in the same topic. Evidence references are topic-local.
- `source_blocks`: `numbered_lines` contains `[original line number, text]`
  pairs. A JSON `json_pointer` plus `value_line` addresses the decoded value,
  not a physical line in its JSON file.
- `source_context` and `evidence` answer modes: answer from the supplied
  originals; an omitted paraphrase is intentional.
- `answer_from_evidence`: join ordered evidence quotations with newlines,
  preceded by `answer_prefix` and a newline when supplied.
- Delta responses, `reused_evidence` and `answer_unchanged`: retain previously
  delivered facts. If that context is unavailable, request `@details` rather
  than guessing what a reference means.

`@details` reads saved detail without new retrieval/model work. It can reject
stale context after source changes; ask the original question again to refresh.
An exact repeat in the same topic can reuse a complete validated answer without
model calls. Follow-ups reuse evidence and consult additional agents as needed;
partial retries retain successful work. Source, configuration, language and
binary changes invalidate reuse as appropriate. Disable reuse with
`memory.cache.enabled: false`.

## Configuration and providers

Settings live in `memory/config.json`. Edit the generated configuration, keeping
profiles still referenced by existing thread bindings. Every profile needs an
explicit model. `memory.chat_agent` falls back to `documents_agent`; verification
uses `verification_agent`, then `preparation_agent`, then `chat_agent`, then
`documents_agent`. Changing the defaults does not rebind existing thread agents.

Supported adapters are `codex`, `kimi`, `claude`, `jsonl`, `ollama`, and
`openai-compatible`. Codex and Kimi have implicit provider entries; other logical
provider names need an explicit `adapter`. CLI adapters require the corresponding
executable and authentication. Codex subprocesses are launched with `--no-daemon`
before the subcommand.

For an HTTP setup, merge a provider and profile like these into the generated
`agent.providers` and `agent.profiles` maps. Replace `MODEL_ID` with your model:

```json
{
  "agent": {
    "providers": {
      "remote": {
        "adapter": "openai-compatible",
        "endpoint": "https://openrouter.ai/api/v1/chat/completions",
        "allow_remote_content": true,
        "api_key_env": "OPENROUTER_API_KEY",
        "max_output_tokens": 8192,
        "response_format": "json_object"
      }
    },
    "profiles": {
      "agent_docs": {
        "provider": "remote",
        "model": "MODEL_ID"
      }
    }
  },
  "memory": {
    "mode": "read_only",
    "chat_agent": "agent_docs",
    "documents_agent": "agent_docs",
    "verification_agent": "agent_docs"
  }
}
```

Remote HTTP endpoints require HTTPS and `allow_remote_content: true`; Ollama
must use a loopback endpoint. A nonblank inline `api_key` takes priority over
`api_key_env`. Inline keys are plaintext configuration: keep them out of version
control. Local unauthenticated endpoints may omit both fields.

`max_output_tokens` defaults to 2048 (range 1–131072) and maps to `max_tokens`
or Ollama `num_predict`. Reasoning may consume that budget. Provider-level
`reasoning_enabled` maps to `reasoning.enabled` or Ollama `think`; Ollama defaults
to off. Profile-level `reasoning_effort` is supported by Codex and OpenAI-compatible
adapters and cannot be combined with HTTP `reasoning_enabled: false`.

OpenAI-compatible providers default to `response_format: "json_object"`.
`"json_schema"` opts into structured output with the task schema; unsupported
schema requests are not silently downgraded. Optional `routing` forwards
OpenRouter-style `order`, `only`, `ignore`, `allow_fallbacks`, `require_parameters`
and `sort` settings as the request's `provider` object. Actual support depends
on the endpoint/model.

| Setting | Default | Meaning |
| --- | --- | --- |
| `memory.mode` | `threads` | Chat remains read-only; `docs_only` additionally excludes threads. |
| `memory.timeout_seconds` | `120` | Overall retrieval deadline, 1–600 seconds. |
| `memory.unified.concurrency` | `3` | Parallel retrieval workers, 1–8. |
| `memory.unified.max_candidates` | `8` | Local root-candidate limit, 1–32. |
| `memory.cache.enabled` | `true` | Allow validated context reuse. |
| `memory.timeouts.ingest_seconds` | unset | Optional ingestion phase limit, capped by the overall timeout. |
| `memory.timeouts.provider_test_seconds` | unset | Optional probe limit, capped by the overall timeout. |
| `memory.agent_retries` | `3` attempts, `180` seconds/attempt, `500` ms backoff | Retry settings within the remaining call deadline. |
| `memory.statistics.enabled` | `false` | Write consolidated usage reports. |
| `memory.agent_logs.enabled` | `false` | Record detailed provider requests and responses. |
| `memory.feedback.enabled` | `false` | Enable diagnostics and deferred error analysis. |

The retired `memory.unified.enabled` key is accepted but has no effect, even when
false. Old coordinator/document phase limits do not control unified retrieval.

All CM HTTP agent and classifier calls can use an explicit proxy:

```json
{
  "agent": {
    "proxy": { "enabled": true, "url": "http://localhost:10809" }
  }
}
```

This includes loopback destinations, overrides environment exclusions, and never
falls back to direct access on failure. TLS verification remains enabled. Proxy
credentials and SOCKS URLs are unsupported. External CLI adapters are rejected
while this proxy is enabled because CM cannot enforce their network routing.

`cm -test_providers` probes each configured profile through the shared adapters,
retries, logging and accounting, without sending documents or chat history.
Unused providers are `not_tested`. Exit code zero requires at least one successful
profile and no failed profiles; `complete: false` can still indicate untested
providers. A probe confirms that minimal request, not every retrieval workflow.

## Saving session memory

Run `cm ingest-session` from the initialized target project. The experimental
Codex importer requires `CODEX_THREAD_ID` (fallback `CODEX_SESSION_ID`) and reads
rollouts under `CODEX_HOME` (default `~/.codex`). It only imports matching
`session_meta.id` values; it never guesses the latest session. Python is not
required. Explicit ingestion writes memory even in `read_only`/`docs_only` modes.

The ingestion agent extracts goals, decisions, requirements, reports and pending
questions into a session root and stable topic threads. Requirement identities
and provenance are host-validated. Add/revise/cancel operations retain omitted
claims; superseded claims go to an archive. Current claims are searchable;
historical archive retrieval through chat is not implemented. Separate sessions
have separate roots; automatic cross-session topic merging is not implemented.

Only new visible messages and selected test/error excerpts are analyzed, together
with compact previous memory. Hidden reasoning, system/developer instructions,
progress messages and tool-call arguments are excluded. Attachments are not
interpreted. Successful CM reads, including cache hits, can be imported once as
advisory read receipts; they do not become user requirements.

Offsets, event IDs and checkpoints make ingestion incremental. No new events
means no ingestion model call. An incomplete final JSONL line waits for the next
run. Frozen batches survive failures; prepared publication is replayed after
interruption. Rollouts are expected to be append-only: truncated files and changed
checkpoint tails fail explicitly, but arbitrary earlier edits are not fully hashed.

Inspect the host-generated `write_receipt`, including on handled failures:

| Receipt status | Meaning |
| --- | --- |
| `saved` | Confirmed changes were published; inspect the listed changes. |
| `unchanged` | No changed memory facts were confirmed. |
| `not_saved` | No change was confirmed. |
| `partially_saved` | Earlier batches committed but a later batch failed. |
| `unknown` | Publication was interrupted; recovery may be needed. |

Command success alone does not prove a requested requirement was saved. Receipts
include the last confirmed revision, claim counts and bounded excerpts; truncation
flags identify omitted detail. Metadata-only changes may have zero claim counts.
Publication spans multiple files, so failures can leave partial projections until
the next recovery. Retry `cm ingest-session` for the same session when
`pending_batch: true`; after `resumed_batch: true, more_events_unchecked: true`,
run it again to check events appended since the batch was frozen.

Selected content is sent to the configured provider. Credential masking is
heuristic, not a guarantee. Extracted reports remain advisory rather than
independent proof. Limits include 64 topics/session, 1200 characters per compact
note, 16 MB per rollout line and 120000 characters per visible message; oversized
input fails without silently consuming the batch.

## MCP and automatic hooks

The MCP server installed by `cm init` runs the current executable with `--mcp`
and the project as its working directory. It exposes `ask` with one string
argument, `question`, through the same memory retrieval path. It does not expose
initialization or ingestion as tools. Each request uses an isolated CM child;
cancellation and deadlines terminate that child. If the executable moves, rerun
`cm init` to update its registered path.

Automatic hooks are opt-in:

```powershell
cm hooks install codex
cm hooks status
# To finish queued imports explicitly:
cm hooks drain
# To remove automatic handlers:
cm hooks uninstall codex
```

Installation updates `.codex/hooks.json`; Codex must trust project hooks before
running them. `SessionStart` reports active CM hooks, `UserPromptSubmit` retrieves
bounded relevant context, and `Stop`/`PreCompact` queue background session imports.
Hook failures report a diagnostic without forcing the coding agent to continue
or abort. Queued imports are not confirmed saves; inspect their receipts under
`memory/runtime/hooks/`. `hooks status` reports the queue, not installation/trust
status. Hook lookups and imports can make model calls.

The managed `AGENTS.md` policy tells the primary agent to reuse supplied evidence
and query only for missing facts, contradictions or necessary freshness checks.
It prefers MCP `ask`, falling back to the CLI when MCP is unavailable. After
meaningful work it calls `ingest-session` once unless active automatic history
hooks were explicitly reported. Explicit remember requests still require an
ingestion receipt. Without hooks, the final answer after ingestion is picked up
by the next import; no duplicate summary/report is needed.

## Diagnostics and storage

Long-running CLI requests emit a flushed `RUNNING` line to stderr every 60 seconds;
this is liveness, not evidence of completion. Wait on the same process. Errors
exit nonzero and print `ERROR` with diagnostics. Interactive input waits emit no
heartbeat, and a failed interactive request ends the process.

Transient connection failures, timeouts and HTTP 408/429/500/502/503/504 can be
retried within the original deadline. Authentication errors, invalid JSON/schema,
policy denial and truncated model output are not transport-retried. Only eligible
fresh read-only calls without native tool activity are replayed. Set
`memory.agent_retries.max_attempts: 1` to disable retries. Provider attempts can
incur usage even if their results are rejected or time out.

| Location | Contents |
| --- | --- |
| `memory/config.json` | Local model/provider and memory settings. |
| `memory/docs/` | User-owned reference documents. |
| `memory/threads/` | Persistent native thread memory. |
| `memory/thread-agents/` | Thread agent bindings and associated persistent state. |
| `memory/runtime/unified/` | Derived indexes, topic context and retrieval traces. |
| `memory/runtime/session-ingest/` | Import checkpoints, frozen batches and revisions. |
| `memory/runtime/session-reads/` | Session-scoped CM read receipts. |
| `memory/runtime/hooks/` | Hook queues, context cache and import receipts. |
| `memory/runtime/mcp/` | MCP child stdout/stderr and request receipts. |
| `memory/runtime/statistics/` | Optional consolidated usage reports. |
| `memory/runtime/agent-logs/` | Optional detailed provider JSONL logs. |
| `memory/runtime/diagnostics/` | Optional event journal and pending error evidence. |
| `memory/feedback/` | Submitted feedback and advisory analysis reports. |

Runtime includes ingestion recovery state, not just disposable caches. Keep it
with persistent memory when preserving incremental ingestion progress.

Statistics report calls, retries, timings, cache events and provider-reported
usage. Missing token/cost counters remain unknown (`null`), not zero. Cached and
reasoning tokens are subsets, not extra totals. `primary_session_usage` is a
cumulative Codex snapshot; do not sum snapshots or add them to CM agent totals.
Interrupted runs may retain reports with `status: running` and partial counts.

Detailed agent logs contain prompts, schemas, responses and HTTP diagnostics.
Automatic feedback also enables detailed logging. Configured credentials are
masked, but logs can contain project/session text and provider reasoning and are
not automatically rotated. Statistics reports omit request/response text.

Enable `memory.feedback.enabled` to journal errors and run a deferred analyst
after at least two pending incidents. It defaults to background operation,
profile `agent_low`, a 60-second timeout and a 300-second retry cooldown. Reports
are advisory: they do not repair code or edit requirements. Failed analysis keeps
incidents pending and does not block the main operation. Manual
`cm feedback "Expected X; observed Y; reproduction steps..."` works without
automatic diagnostics and accepts up to 8000 characters.

## Publishing and updating releases

The release workflow is configured for `gangelxxx/climemory` and the npm package
`@gangelxxx/climemory`. It builds Windows x64, Linux x64/ARM64 and macOS Intel/Apple Silicon
binaries, publishes them to GitHub Releases, then publishes the npm installer.
The npm package includes the release's checksums and downloads only the binary
needed by the user's platform.

### First publication

Before automatic npm publishing can work:

1. Push the full project, including the Rust sources and both npm workflows, to
   GitHub. The workflows need `Cargo.toml`, `Cargo.lock`, `src/`, `tests/`, README,
   `package.json`, `bin/`, and `npm-tools/` in the repository.
2. Run the **npm release** workflow manually on the default branch with
   **publish_npm unchecked**. This creates the binary release and the
   **npm-package** workflow artifact without publishing to npm.
3. Download and extract that artifact, then publish its prepared `.tgz` from
   your npm account. For the current version:

   ```powershell
   npm login
   npm publish ./gangelxxx-climemory-2.62.1.tgz --access public
   ```

4. In the npm package settings, configure a GitHub Actions Trusted Publisher:
   owner `gangelxxx`, repository `climemory`, workflow `npm-release.yml`, no
   environment name. Enable direct publication if the setting is offered.

Subsequent releases use OIDC without a stored npm token. Do not publish the raw
source checkout: its binary checksum manifest is generated during release
preparation.

### Subsequent releases

Set the same new stable `x.y.z` version in `Cargo.toml` and `package.json`, then
refresh `Cargo.lock`. For example, after changing both manifests to `2.63.0`:

```powershell
cargo check
npm run release:check
git add Cargo.toml Cargo.lock package.json
git commit -m "2.63.0 npm release"
git push
```

The automatic trigger is **`npm release` in the last commit of a push to the
default branch**. With squash merges, put it in the final squash commit. The
commit message triggers the workflow; it does not change the version. Ordinary
commits and pushes to other branches do not publish.

The workflow runs Rust checks, installer tests and actual installation checks on
each platform before publication. It verifies installation from the packed npm
artifact against the public GitHub Release before publishing npm. A failed run
can be retried for the same commit: public binaries are reused, an already
published npm version is skipped, and moving npm `latest` backwards is rejected.

## Development

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo test --no-default-features
cargo build --release --bin cm
```

Check the npm installer and release metadata with:

```powershell
npm test
npm run release:check
npm pack --dry-run
```

The installer tests cover verification failures, managed updates, installation
locks, release retries and running the packed CLI through npm. To test actual
initialization without model calls, run `node npm-tools/smoke.cjs` with the path
to a built CM binary; it creates and removes its own temporary project.

`scripts/verify.ps1` runs the formatting, Clippy and both test configurations.
The default `code-index` feature includes the code-index/parser dependencies;
it does not add code-search commands to the public chat. `internal-test-cli`
builds `cm-internal-tests`, a separate harness for the legacy command interface.
Public chat, ingestion, MCP and hook tests live in `tests/`.

Use the direct Cargo build above for the current chat executable.
`scripts/build.ps1` and `scripts/build.bat` bump the version and invoke the local
installer; `scripts/install-local.ps1` still validates legacy `version` and
`help context` commands, so it is not a compatible installation path for the
current public CLI. The repository code-search workflow also retains the
experiment's restriction: leave the frozen repository binary unchanged and
deploy experimental builds only to the test calculator.

Python experiments and live benchmarks are separate from the Rust application.
See [the Python experiment guide](python_scripts/README.md) for session extraction
and lifecycle tools. They may require local fixtures, configured providers and
paid model calls; they are not substitutes for the deterministic Cargo checks.
