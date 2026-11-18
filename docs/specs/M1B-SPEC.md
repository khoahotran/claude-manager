# M1b + M2 spec - search, metadata, live refresh, session viewer (CONTRACT between 4 parallel agents)

Goal: DELIVER FAST. Re-use MIT-licensed reference code wherever possible instead of designing from scratch. Reference code came from
local clones of the MIT reference repos (see THIRD-PARTY.md)
(history-viewer = Rust/Tauri backend, MIT; ccusage = Rust, MIT; claude-code-viewer, claude-devtools, klovi = TypeScript, MIT).
Rules: MIT code may be ported/adapted (translate TS->Rust is fine); keep it attributable: append an entry to
[`THIRD-PARTY.md`](../../THIRD-PARTY.md) (create if missing; re-read before editing) with: our file, source repo + path, license, copyright
line from its LICENSE. NEVER copy from opcode (AGPL). Don't paste big license-incompatible blobs; rewrite to our types/security model.
Security model is unchanged (see [`KNOWN-ISSUES.md`](../planning/KNOWN-ISSUES.md)): read-only GETs need the cookie or X-CM-Token; every PUT/POST/DELETE needs
X-CM-Token + application/json. New routes must live under /api/ so the middleware classifies them correctly. Never mutate Claude's own
.jsonl files. All new on-disk state goes under `<app_dir>` (next to the exe, `CM_HOME` in tests).

## Ownership (no two agents edit the same file)
- SEARCH agent: new `src/search.rs`. Exposes `pub fn router() -> axum::Router<AppState>`.
- DATA agent: `src/parse.rs`, `src/index.rs`, `src/api.rs`, `src/usage.rs`, `src/pricing.rs`, new `src/meta.rs`, new `src/watch.rs`;
  `src/state.rs` APPEND-ONLY (new fields/types; never change existing JSON).
- VIEWER agent: new `src/viewer.rs` (+ `src/diff.rs` if needed). Exposes `pub fn router() -> axum::Router<AppState>`. Reads session files
  ON DEMAND with its own line reader (do not edit parse.rs). Looks sessions up through `state.index` - DATA agent keeps a stable
  `Index::session_path(&self, id:&str) -> Option<PathBuf>` (add it if missing; VIEWER may rely on it; if it doesn't exist yet, add it
  to index.rs as a tiny pub method in YOUR copy and tell the lead in the report).
- WEB agent: everything under `web/` except `web/vendor/`.
- Shared files: `src/main.rs` (only `mod x;` lines and `.merge(x::router())` lines), `Cargo.toml` (append deps; RE-READ right before editing;
  keep the file otherwise untouched), `THIRD-PARTY.md` (append). Lead integrates at the end.

## 1) Search (GET /api/search)
`GET /api/search?q=<text>&source=<name>&project=<cwd-or-projectKey>&from=YYYY-MM-DD&to=YYYY-MM-DD&role=user|assistant&limit=30&offset=0`
```jsonc
{ "q":"...", "total":123,
  "hits":[ { "id":"<session id as in /api/sessions>", "sessionId":"..", "source":"..", "title":"..", "cwd":"..",
             "ts":"2026-11-15T10:00:00.000Z", "role":"user", "snippet":"...text around the match...",
             "matches":[[12,17]] } ],            // [start,end) CHAR offsets into snippet (UI highlights; no HTML)
  "index":{ "running":false, "filesDone":0, "filesTotal":0, "docs":0 } }
```
Implementation: SQLite FTS5 (rusqlite with feature `bundled`) in `<app_dir>/search.db`, trigram tokenizer (substring match, works for
Vietnamese/CJK; for queries < 3 chars fall back to LIKE on a bounded set), BM25 ranking, user hits slightly boosted, only user + assistant TEXT
blocks (cap 10 KB per block; skip tool I/O, thinking, meta/system-reminder text), incremental by (size, mtime) per file, background thread,
never blocks requests, safe with archived/trashed files (status in the row so results can mark/filter them; default: active+archived).
Reference: claude-code-viewer `SearchService.ts` (MIT), history-viewer `commands/session/search.rs` (MIT).

## 2) Metadata sidecar + live refresh + normalized projects (DATA agent)
New `Summary` fields (append): `customTitle?:string`, `tags:string[]`, `note?:string`, `starred:bool`, `projectKey:string`,
`projectName:string`, `repoRoot?:string`, `liveState:"running"|"idle"`, `continuesFrom?:string`, `continuedBy?:string`.
- `title` precedence: customTitle (sidecar) > ai-title > summary record > first genuine user prompt (skip caveat/Warmup/command wrappers) >
  "/cmd args" > "(empty session)".
- `projectKey`: normalized cwd so the same repo seen from Windows and WSL groups together (`C:\x` == `/mnt/c/x` == `\\wsl$\distro\mnt\c\x`;
  lower-case drive; forward slashes; trailing slash removed). `projectName` = last path component. `repoRoot`: worktree grouping via `.git` file
  `gitdir:` pointer (read through the source's filesystem; tolerate failure) - reference klovi `resolve-worktree.ts`, history-viewer project tree.
- `liveState`: "running" if the file was written < 5 min ago (or an unfinished turn), else "idle".
- Continuation chain via `compact_boundary.logicalParentUuid` (history-viewer `session/chain.rs`): fill `continuesFrom`/`continuedBy`
  (session ids). Do not hide anything.
- `GET /api/meta`:

  ```jsonc
  { "sessions": { "<id>": { "title": "..", "tags": ["a"], "note": "..", "starred": true } } }
  ```

- `PUT /api/meta`, body (omitted key = unchanged, `title: null` clears):

  ```jsonc
  { "id": "<id>", "title": "x" | null, "tags": ["a"], "note": "..", "starred": true }
  ```

  Response:

  ```jsonc
  { "ok": true, "meta": { ... } }
  ```

  Limits: title<=200, <=20 tags of <=40 chars, note<=10000. Stored atomically in `<app_dir>/meta.json`, keyed by
  session id (stable across archive/trash/restore).
- `GET /api/changes` -> `{"gen":123,"scan":{...ScanState...}}`; `gen` increases whenever the index changes (watcher event, action, rescan).
  File watcher (`notify` + ~300 ms debounce + fingerprint (len,mtime) + periodic catch-up scan; on Windows strip `\\?\` from event paths;
  `\\wsl$` shares do not emit events -> poll those every ~15 s). Changed files are re-parsed incrementally.

## 3) Session viewer API (VIEWER agent)
`GET /api/session/{id}/turns?offset=0&limit=100[&agent=<agentId>]` (agent = subagent transcript)
```jsonc
{ "id":"..", "total":412, "offset":0,
  "subagents":[ {"agentId":"a1b2","toolUseId":"toolu_..","description":"..","type":"Explore"} ],
  "turns":[ { "idx":0, "kind":"prompt|reply|command|meta|error", "role":"user|assistant|system", "ts":"ISO",
              "model":"claude-opus-5-5"|null, "usage":{"in":0,"out":0,"cc":0,"cr":0}|null, "durationMs":null,
              "blocks":[ Block, ... ] } ] }
Block = {"type":"text","text":".."}
      | {"type":"thinking","text":".."}                               // empty/redacted thinking is omitted
      | {"type":"tool","id":"toolu_..","name":"Bash","kind":"shell|file_read|file_write|file_edit|search|web|subagent|skill|mcp|todo|generic",
         "summary":"one line","input":{...},"inputTruncated":false,
         "result":{"text":"..","isError":false,"truncated":false}|null,           // text <= 20000 chars, images -> "[image]"
         "diff":{"path":"..","hunks":[{"oldStart":1,"oldLines":3,"newStart":1,"newLines":4,"lines":[" ctx","-old","+new"]}]}|null,
         "agentId":"a1b2"|null, "durationMs":1200|null}
      | {"type":"meta","subtype":"compact_boundary|api_error|turn_duration|stop_hook|local_command|interrupted|bash","text":"..","data":{...}}
      | {"type":"parseError","line":123,"error":"..","raw":"first 500 chars"} }
```
Rules ([`RESEARCH.md`](../research/RESEARCH.md)): assistant lines + tool_result-only user lines = one turn; pair tool_use.id <-> tool_result.tool_use_id (orphans stay with
result:null); skip isMeta, `<system-reminder>`, `<local-command-caveat>`; slash commands -> kind "command" with text "/name args"; merge
`<bash-input>` + `<bash-stdout/stderr>`; real line diff for Edit/MultiEdit/Write (use the `similar` crate); subagent link via
`toolUseResult.agentId` (+ `<session>/subagents/agent-<id>.jsonl`, `.meta.json` toolUseId); tool kinds table; unknown line/type -> never crash.
Pagination by turn index; big sessions must not be fully materialised per request (index byte offsets of turn starts on first request, cache in
memory per session keyed by (size,mtime)). `GET /api/session/{id}/export?format=md|json` returns a download (`Content-Disposition`).
Reference: klovi `plugin-claude-code/src/parser.ts` + tool normalizers, claude-code-viewer conversation schema/toolVisualizers,
history-viewer message rendering rules, claude-devtools `ChunkBuilder`/linking.

## 4) Web UI (WEB agent)
- Ctrl/Cmd+K search palette -> /api/search (debounce 200 ms, highlight via `matches`, arrow keys, Enter opens the session in the viewer, filter
  chips: source/role/date, shows "indexing..." from `index`).
- Session drawer -> new Viewer on /api/session/{id}/turns: turn cards (prompt / reply / command / meta / error), collapsible tool cards (one-line
  summary, kind icon, visual/raw toggle, Bash output, Edit/Write diff rendering, orphan marker), thinking collapsed, subagent click-through
  (loads `?agent=`), token/duration footer per turn, prompts-only filter, Alt+Up/Down jump between prompts, in-page find (Ctrl+F inside the viewer),
  infinite scroll/virtualised paging, export buttons (md/json). Keep the old drawer working if the new endpoint 404s (fallback to /api/messages).
- Rename (customTitle), tags, note, star via /api/meta (inline editors in the drawer + star/tag columns/filters in the table); group-by project using
  `projectKey`/`projectName`; badges for `liveState` running and continuation links (`continuesFrom`/`continuedBy`).
- Auto refresh: poll `/api/changes` every 5 s while the tab is visible; refetch /api/sessions only when `gen` changed.
- Language: Vietnamese at the time of writing (now translatable, see `web/locales/`); light/dark; 390px responsive; escape all untrusted strings; no new frameworks.

## Acceptance (each agent proves its part)
Unit/integration tests for your module; real-data smoke test (read-only) with timings; no regression of existing endpoints (existing
57 tests keep passing); report what you ported from which repo (and THIRD-PARTY.md entries). Build/test in a private WSL copy
(`rsync -a --delete --exclude target /mnt/c/tools/claude-manager/ ~/work/<name>/`), copy ONLY your owned files back. Test servers on your
own port: SEARCH 8791, DATA 8792, VIEWER 8794, WEB mock 8793 (never 8765, the user's real instance runs there).
