# src - Rust backend

One crate, one binary (`claude-manager`). Web assets from [`../web`](../web/README.md) are embedded at build time.
Every route passes through `security::guard` (Host / Origin / token checks). Design and contract:
[`../docs/architecture/DESIGN.md`](../docs/architecture/DESIGN.md).

| File | Purpose | Key routes |
|---|---|---|
| `main.rs` | Entry point: CLI args, AppState, router assembly (merges every module router), starts watcher/search, opens the browser | - |
| `state.rs` | `AppState` and shared types (session `Summary`, `Source`, scan state); the JSON shapes the UI depends on | - |
| `config.rs` | Load `config.json`; auto-detect sources (native + WSL distros) | - |
| `security.rs` | Host / Origin / token guard (constant-time compare), `cm_auth` cookie rules, applied to all routes | - |
| `web.rs` | Serves the embedded `web/` folder; one-time `?k=` login, token injection into `index.html`, "locked" page | `/`, static files |
| `api.rs` | Session, usage and workspace REST handlers (blocking work off the async executor) | `/api/sessions`, `/api/messages`, `/api/rescan`, `/api/action`, `/api/sources`, `/api/folders`, `/api/workspace`, `/api/meta`, `/api/changes`, `/api/usage`, `/api/blocks`, `/api/pricing`, `/api/expiring` |
| `index.rs` | Scan, binary cache, in-memory index, archive/trash/restore/purge, continuation chains, title sync (`custom-title` append), frozen history hand-off | (used by `api.rs`) |
| `parse.rs` | Streaming JSONL session parser; compact usage events with dedupe; tool/skill/subagent/error counters | - |
| `meta.rs` | Sidecar metadata (title, tags, note, star) in `meta.json`, atomic writes | `/api/meta` (via `api.rs`) |
| `watch.rs` | Live refresh: notify watcher with debounce, 15 s polling for `\\wsl$`, bumps the change generation | - |
| `usage.rs` | Global dedupe, usage rows, heatmap, tools, active time, 5-hour blocks, frozen history (`history.bin`) | `/api/usage`, `/api/blocks` (via `api.rs`) |
| `pricing.rs` | Price table from `pricing_snapshot.json` plus user overrides; cost of one request | `/api/pricing` (via `api.rs`) |
| `pricing_snapshot.json` | Embedded LiteLLM price snapshot (data, not code) | - |
| `search.rs` | SQLite FTS5 full-text index in `search.db`, incremental background indexing | `/api/search` |
| `viewer.rs` | On-demand session viewer: byte-offset turn index, tool/diff blocks, subagents, export | `/api/session/{id}/turns`, `/api/session/{id}/export` |
| `term.rs` | PTY manager (portable-pty): spawn `claude` from argv, scrollback ring, resize, kill | - |
| `ws.rs` | Terminal REST and WebSocket bridge | `/api/term`, `/api/term/{id}`, `/api/term/{id}/resize`, `/ws/term/{id}` |
| `settings.rs` | UI/source settings (`ui.json`, `config.json`), model/effort lists, open-in-editor | `/api/settings`, `/api/open-editor` |
| `vscode.rs` | Find the `.code-workspace` a session belongs to | - |
| `inventory.rs` | Read-only scan of skills / plugins / MCP names / hooks / CLAUDE.md (secrets redacted) | `/api/inventory` |
| `speckit.rs` | spec-kit progress per repo / worktree / feature plus linked session cost | `/api/speckit` |

Ported MIT code is attributed in [`../THIRD-PARTY.md`](../THIRD-PARTY.md). Tests are inline `#[cfg(test)]` modules
(see [`../tests/README.md`](../tests/README.md)).
