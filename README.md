# claude-manager

[![CI](https://github.com/khoahotran/claude-manager/actions/workflows/ci.yml/badge.svg)](https://github.com/khoahotran/claude-manager/actions/workflows/ci.yml)
[![License](https://img.shields.io/github/license/khoahotran/claude-manager)](LICENSE)
![Platform](https://img.shields.io/badge/platform-Windows%20%2B%20WSL-blue)

A single-binary (Rust) local web app that manages every Claude Code session on a machine (Windows + WSL) and runs
`claude` in parallel terminal panes. The UI is served from the binary itself (no Node, no install step).

## Features
- **Sessions**: one table across Windows and WSL sources; filter, sort, group by project, star/tag/note/rename, bulk
  archive / trash / restore / purge (never silent deletion), full session viewer (tool cards, diffs, subagents, export).
- **Search**: global full-text search (Ctrl+K) over user and assistant text, backed by SQLite FTS5.
- **Usage**: tokens and estimated cost by day / project / model / source, 5-hour billing blocks, heatmap, tool / skill /
  subagent stats, cache hit rate, history that survives Claude Code's log cleanup.
- **Workspace**: several real `claude` terminals side by side (Windows ConPTY, WSL via `wsl.exe`), resume from a session,
  panes survive page reloads, pick model/effort, open the folder or workspace in VS Code.
- **Inventory**: skills, plugins, MCP servers, hooks, CLAUDE.md sizes, and a spec-kit progress dashboard.
- **Settings and i18n**: sources, icon/text mode, what to show, light/dark theme, Vietnamese and English UI.
  The UI starts in Vietnamese; switch to English in Settings (the choice is remembered).

Feature status and roadmap: [`docs/planning/FEATURES.md`](docs/planning/FEATURES.md).

## Screenshots
_To be added._ Screenshots must be taken with demo data (no real session titles, paths or tokens).

## Quick start
Requirements: Windows (WSL optional, for Linux-side Claude sessions), and to build from source Rust (rustup) plus
Visual Studio Build Tools (C++).

```text
build.cmd      release build -> bin\claude-manager.exe (stop a running instance first; Windows locks the exe)
start.cmd      run bin\claude-manager.exe; opens the browser, or re-opens the running instance
```
State files (`config.json`, `cache.bin`, `history.bin`, `meta.json`, `ui.json`, `workspace.json`, `search.db`) live next
to the exe in `bin\`. Sources (Windows and WSL `.claude` folders) are auto-detected on first run and editable in Settings or
`bin\config.json`. Default port: 8765.

Tests: `cargo test` (on Windows or Linux/WSL), plus the UI tests in [`tests/`](tests/README.md).

## Security model
- Listens on `127.0.0.1` only; requests with a wrong Host or Origin are rejected (DNS rebinding / cross-site protection).
- A random per-run token protects every write, terminal and WebSocket call; the page is only served to a client that
  logged in through the one-time `?k=<token>` link (HttpOnly, SameSite=Strict cookie).
- Claude's own `.jsonl` files are never rewritten (only a `custom-title` line may be appended to an idle session on
  rename); the app keeps its own cache and sidecar files; processes are started from argv, never through a shell.

Details: [`docs/architecture/DESIGN.md`](docs/architecture/DESIGN.md), open items in
[`docs/planning/KNOWN-ISSUES.md`](docs/planning/KNOWN-ISSUES.md).

## Project structure

```text
claude-manager/
  README.md          this file
  THIRD-PARTY.md     attribution for MIT code ported into src/ (licensing convention: kept at the root)
  Cargo.toml         crate manifest (Cargo.lock: locked dependencies)
  build.cmd          release build into bin\
  start.cmd          launcher (runs bin\claude-manager.exe)
  bin/               built exe plus runtime state (config.json, caches, sidecars); not source
  src/               Rust backend: API, parser, index, search, usage, terminals    -> src/README.md
  web/               frontend (plain JS/CSS/HTML), embedded into the exe           -> web/README.md
    vendor/          xterm.js + fit addon (vendored, offline) and its license
    locales/         UI translations (vi.json, en.json)                            -> web/locales/README.md
  tests/             test notes and UI acceptance tests                           -> tests/README.md
    uat/             headless-Edge UAT of the Settings dialog                      -> tests/uat/README.md
  docs/              all documentation                                            -> docs/README.md
    architecture/    design and contract
    planning/        features/roadmap, known issues
    specs/           milestone specifications
    research/        research digests
```

## Contributing
See [CONTRIBUTING.md](CONTRIBUTING.md) and the [Code of Conduct](CODE_OF_CONDUCT.md). Changes are listed in [CHANGELOG.md](CHANGELOG.md).

## Security
See [SECURITY.md](SECURITY.md) for the threat model and how to report a vulnerability privately.

## License
See [LICENSE](LICENSE) (copyright claude-manager contributors). Third-party code and its licenses: [THIRD-PARTY.md](THIRD-PARTY.md).
