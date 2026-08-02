# Codex Resume Tracker

A Linux-first, local-first Rust TUI for finding a past Codex thread and resuming it in a new terminal without closing the tracker.

`codex-resume-tracker` uses Codex's documented [`SessionEnd` hook](https://developers.openai.com/codex/hooks) for fast capture and the supported [`thread/list` and `thread/read` App Server APIs](https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md) for metadata and visible conversation messages.

## Features

- Search generated/manual titles, summaries, directories, source, model metadata, tags, and notes as you type.
- Keep pinned threads before the normal recency ordering.
- Display the exact `codex resume <thread-id>` command and original working directory.
- Press **Enter** to spawn `x-terminal-emulator -e codex -C <cwd> resume <id>` as an argv vector. There is no shell interpolation, and the tracker stays open.
- Edit title and summary overrides, notes, and comma-separated tags; retry summaries and inspect status/errors.
- Import CLI and VS Code/IDE threads by default. `codex exec` history is opt-in.
- Store everything locally in SQLite. Raw conversation messages are used transiently and are never written to the tracker database.
- Never archive or delete a Codex thread. v0.1 has no delete action, transcript viewer, cloud sync, or archived-thread management.

## Requirements

- Linux with a terminal emulator exposed as `x-terminal-emulator`, or a configured alternative.
- Codex CLI **0.144.1 or newer** with App Server support.
- `setsid` from util-linux for the detached hook worker.
- Rust 1.82+ when building from source.
- For OpenAI API summaries only: `OPENAI_API_KEY` in the launch environment.

Run `codex-resume-tracker doctor` after setup to validate every requirement.

## Build and install

```bash
git clone git@github.com:G-lacier/codex-resume-tracker.git
cd codex-resume-tracker
cargo build --release --locked
install -Dm755 target/release/codex-resume-tracker "$HOME/.local/bin/codex-resume-tracker"
codex-resume-tracker setup
```

The release workflow also publishes a Linux x86_64 tarball and SHA-256 checksum for tagged private releases.

## First-run setup

Run:

```bash
codex-resume-tracker setup
```

The wizard confirms each choice. Defaults are:

| Choice | Default |
| --- | --- |
| Summary provider | Ephemeral Codex |
| Summary input | 64K Unicode characters |
| On close | Automatic saved provider |
| Sources | CLI + VS Code/IDE; exclude `codex exec` |
| Hook | Merge into `~/.codex/hooks.json` |

Setup always prints the exact hook JSON for review. Automatic setup structurally merges one idempotent `SessionEnd` handler and preserves all existing hook events, groups, handlers, and unrelated JSON. It will refuse malformed/incompatible JSON rather than overwrite it. Manual and skip modes do not change the hook file.

After installing or changing the hook, start Codex and run **`/hooks`**. Codex will not execute a non-managed command hook until you review and trust its exact definition. The hook has the documented three-second maximum timeout; it performs a fast SQLite upsert, queues enrichment, starts a detached worker, and exits.

Non-interactive defaults are available when desired:

```bash
codex-resume-tracker setup --defaults
codex-resume-tracker setup --defaults --provider local --input 16k \
  --on-close local-first --source cli,vscode --hook manual --no-import
```

The initial import reads every matching non-archived thread and generates **local extractive** metadata only. It never invokes Codex or the OpenAI API. Setup then offers explicit on-demand or summarize-all AI enrichment and warns before provider usage.

## Commands

```text
codex-resume-tracker                     Launch the TUI
codex-resume-tracker setup               Run setup and initial local import
codex-resume-tracker sync                Import/update matching threads locally
codex-resume-tracker summarize <id>      Regenerate one summary
codex-resume-tracker summarize --all     Confirm, then regenerate all summaries
codex-resume-tracker doctor              Validate the complete installation
```

Provider and input overrides apply to one summarize command:

```bash
codex-resume-tracker summarize thr_123 --provider local --input 16k
codex-resume-tracker summarize --all --provider codex --input entire
codex-resume-tracker summarize --all --provider openai --input 32768 --yes
```

`--yes` is intended for deliberate automation. Without it, summarize-all requires confirmation.

### TUI keys

| Key | Action |
| --- | --- |
| `j`/`k`, arrows | Select a thread |
| `/` | Incremental search; Enter/Esc finishes |
| Enter | Resume selected thread in a new terminal |
| `s` | Sync and run local extraction |
| `r` | Retry selected summary; AI providers require confirmation |
| `A` | Confirm and summarize all visible threads |
| `t` / `y` | Edit title / summary override |
| `n` / `g` | Edit notes / tags |
| `p` | Toggle pin |
| `v` / `c` | Cycle saved provider / input cap |
| `q` or Ctrl-C | Quit tracker |

## Summary providers

### Local extractive

No network or model usage. The first visible user request becomes the title, and the latest visible user/assistant message becomes the summary.

### Ephemeral Codex

Runs a separate `codex exec` with:

- `--ephemeral` so no summarizer thread is saved;
- user config ignored and hooks disabled;
- read-only sandboxing and ignored exec-policy rules;
- a strict JSON output schema.

This consumes Codex model usage. Provider failures are stored and displayed; the tracker never silently switches provider or changes the selected input policy.

### OpenAI API

Calls `POST /v1/responses` using the model in config (default `gpt-5-mini`), strict Structured Outputs, and `store: false`. The API key is read **only** from `OPENAI_API_KEY`; it is never placed in TOML, SQLite, or logs. API calls may incur charges and remain subject to the account's OpenAI data controls and retention terms.

## Input limits and privacy

Only visible `userMessage` text and `agentMessage` text returned by App Server are considered. Reasoning, tool output, hook prompts, images, system content, and developer content are excluded.

- `16k`, `64k`, and custom positive limits sample representative beginning, middle, and end messages.
- `entire` sends every extracted visible text message. It does not silently truncate.
- Generated title and summary whitespace is normalized and each field is hard-limited to 240 Unicode characters.

The in-memory message vector is dropped after enrichment. Tracker persistence contains thread identity, resume command, cwd, source/model metadata when App Server supplies it, timestamps, normalized generated/manual text, notes, tags, pin state, and job/error status. It does **not** contain transcript paths or raw thread messages.

## Storage and configuration

XDG paths are honored:

```text
$XDG_DATA_HOME/codex-resume-tracker/tracker.sqlite3
$XDG_CONFIG_HOME/codex-resume-tracker/config.toml
$XDG_STATE_HOME/codex-resume-tracker/worker.log
```

When an XDG variable is unset, the standard fallbacks are `~/.local/share`, `~/.config`, and `~/.local/state`.

Example config:

```toml
version = 1
summary_provider = "codex"
summary_input = "64k"
on_close = "automatic"
sources = ["cli", "vscode"]
terminal_argv = ["x-terminal-emulator", "-e", "codex", "-C", "{cwd}", "resume", "{thread_id}"]
openai_model = "gpt-5-mini"
```

`terminal_argv` must contain both `{cwd}` and `{thread_id}`. Each array entry stays a single OS argument, including values containing spaces or shell metacharacters.

## Trust and failure behavior

- Review the merged JSON and approve it with `/hooks`; setup never grants hook trust itself.
- Existing hook definitions are retained, and repeated setup does not add duplicates.
- The hook output cannot steer Codex and the worker never edits a thread.
- App Server, provider, and launch errors are surfaced in the CLI/TUI and stored as status; there is no silent provider fallback.
- SQLite uses foreign keys, WAL mode, a busy timeout, and idempotent migrations/upserts for concurrent SessionEnd events.

## Development and release checks

Keep all project artifacts under `./TMP`:

```bash
mkdir -p TMP
export TMPDIR="$PWD/TMP"
export CARGO_HOME="$PWD/TMP/cargo-home"
export CARGO_TARGET_DIR="$PWD/TMP/target"
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo build --release --locked
```

Tests cover migrations and concurrent access, hook ingestion/merging, App Server pagination and extraction, provider request/response contracts, representative sampling, source filters, TUI actions, terminal argv safety, and a complete fixture lifecycle through a captured fake terminal.

## License

MIT

