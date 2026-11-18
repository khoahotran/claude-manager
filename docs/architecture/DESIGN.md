# claude-manager - design and contract

Single-binary (Rust) local app: manage every Claude Code session (Windows + WSL) and run Claude
in parallel terminal panes. Replaces the Node prototype at `C:\tools\claude-session-manager`
(kept as a behavioural reference: `server.js`, `public/index.html`).

The current module map lives in [`src/README.md`](../../src/README.md) and the UI file map in
[`web/README.md`](../../web/README.md). This document is the original design contract plus the security rules.

## Hard rules
- Libraries from crates.io only. Some logic is ported from MIT-licensed reference projects with attribution (see [`THIRD-PARTY.md`](../../THIRD-PARTY.md)); nothing is copied from opcode (AGPL).
  Ported MIT code is tracked in [`THIRD-PARTY.md`](../../THIRD-PARTY.md).
- No git repo.
- Binds `127.0.0.1` only. Every state-changing / terminal endpoint is authenticated (see Security).
- Never delete user data silently: archive/trash move files; only `purge` on trashed items deletes.

## Layout (original core modules)

```text
Cargo.toml
src/main.rs        entry: CLI args, AppState, router, open browser
src/state.rs       AppState + shared types (Session summary, Source, ...)   (frozen contract)
src/config.rs      config.json load/detect sources (Windows + WSL distros)
src/parse.rs       JSONL session parser (fast path, no full JSON for noise)
src/index.rs       scan/cache/index + archive/trash/restore/purge actions
src/api.rs         REST: /api/sessions /api/messages /api/rescan /api/action
src/security.rs    Host/Origin/token checks, axum middleware
src/term.rs        PTY manager (portable-pty), scrollback ring, spawn specs
src/ws.rs          /ws/term/{id} websocket + /api/term REST
web/index.html     shell + Sessions + Usage + Workspace tabs
web/app.js, web/workspace.js, web/style.css
web/vendor/        xterm.js + fit addon (vendored, offline)
```
Static files are embedded in the binary (rust-embed), served from `/`.

## Sources and status roots (same as the prototype)
`config.json` next to the exe:

```jsonc
{
  "sources": [ { "name", "dir", "distro?", "claude?" } ],
  "port": 8765
}
```

Per source dir D: active `D/projects`, archived `D/_manager/archive`, trashed `D/_manager/trash`.
`distro` (WSL only) is used to launch terminals via `wsl.exe -d <distro> --cd <linux cwd> -- <claude>`.
`claude` = optional explicit path to the claude executable for that source; if absent it is auto-detected
(Windows: PATH, then newest `%USERPROFILE%\.vscode\extensions\anthropic.claude-code-*\resources\native-binary\claude.exe`;
WSL: `command -v claude`, then newest `~/.vscode-server/extensions/anthropic.claude-code-*/resources/native-binary/claude`).

## REST contract (JSON)
```text
GET  /api/sessions   -> { sources:[name], scan:{running,total,done,startedAtMs,finishedAtMs,errors}, sessions:[Summary] }
Summary = { id, source, status:"active|archived|trashed", projDir, sessionId, title, firstPrompt, lastPrompt,
            cwd, branch, first, last, users, assistants, tools, models:{model:count}, tok:{i,o,cr,cc},
            daily:{ "YYYY-MM-DD": { model:[in,out,cacheWrite,cacheRead] } }, version, entrypoint, size }
            (identical to the Node prototype so the UI logic ports 1:1)
GET  /api/messages?id=   -> { items:[{role:user|assistant|tool|result|meta, text, name?, model?, ts?}] }
POST /api/rescan         -> { started:true }
POST /api/action {action:archive|trash|restore|purge, ids:[]} -> { results:[{id,ok,error?}] }
GET  /api/sources        -> [{ name, dir, distro?, claudePath? }]   (for the Workspace folder picker)
GET  /api/folders?source=&q=  -> distinct cwd folders known from sessions (for the "open folder" picker)
```

Terminals (authenticated):
```text
POST   /api/term         {source, cwd, resume?: sessionId, args?: [string], cols, rows} -> { id, title }
GET    /api/term         -> [{ id, source, cwd, title, alive, createdMs }]
DELETE /api/term/{id}    kill + forget
POST   /api/term/{id}/resize {cols,rows}
WS     /ws/term/{id}     server->client: binary frames = raw PTY output (replays scrollback first);
                         client->server: binary = keystrokes; text JSON {"type":"resize","cols":N,"rows":N}
```
PTYs outlive page reloads; they die on DELETE, process exit, or app exit.
The workspace layout is persisted via GET/PUT `/api/workspace` (opaque JSON blob, stored in `workspace.json`).

## Security (non-negotiable)
- Reject any request whose Host header is not `127.0.0.1:<port>` or `localhost:<port>` (DNS rebinding).
- Startup generates a random 256-bit token. It is injected into the served index.html (meta tag), which is
  only served for an allowed Host. All POST/PUT/DELETE and all /ws and /api/term* require the header `X-CM-Token`
  (WS: token via `Sec-WebSocket-Protocol` subprotocol or `?t=`), compared in constant time.
- WS additionally requires `Origin` == `http://127.0.0.1:<port>` or `http://localhost:<port>`.
- File actions operate only on paths resolved from the index (never from client input) and verify the path is
  inside the source dir.
- Terminal `cwd` must be an existing directory; `args` are passed as argv, never through a shell.

Later hardening, after the independent review: a cookie login flow so the token is never in a public page. See
[`KNOWN-ISSUES.md`](../planning/KNOWN-ISSUES.md).

## Performance targets
- Cold scan of a few hundred sessions / a few hundred MB over 9P: dominated by IO; use rayon + memmap, skip noise records by
  byte-prefix without JSON parse. Warm start (cache hit): stat only, < 1 s for 250 files on NTFS.
- Cache: binary (bincode) at `cache.bin` next to the exe, keyed by (path, mtime, size).
- The server is async (tokio); parsing runs on rayon/blocking threads, never on the async executor.

## Acceptance (must match the Node prototype on real data)
- Same session count per source and same total output tokens (+-0) as `GET /api/sessions` of the Node app.
- Archive -> trash -> restore -> purge works on a fake `.claude` tree including the companion `<sessionId>/` dir; purge refuses non-trashed.
- Requests with a wrong Host / missing token / bad Origin get 403; terminals are unusable without the token.
- Terminal: spawn `claude` (or `cmd` for tests) in a folder, type, resize, reload the page, scrollback replays, kill.
