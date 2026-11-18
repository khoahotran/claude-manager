# Known issues / backlog from the independent review

Fixed after the first review (2026-11-16): token no longer served to unauthenticated clients (cookie login flow),
resume id cannot start with `-`, archive/trash refused for recently-written sessions, bounded line length in the
parser, `panic = "abort"` removed (a panic now only kills the request/thread, not every terminal).

## Open
1. **Windows process cleanup (low, runtime unverified).**
   - `tokio::signal::ctrl_c` only covers Ctrl+C; closing the console window / logoff / shutdown do not run `kill_all`.
     Fix: handle `ctrl_close`, `ctrl_logoff`, `ctrl_shutdown` (tokio::signal::windows).
   - `kill()` terminates only the direct child (`claude.exe`, `cmd.exe`, `wsl.exe`); grandchildren (MCP servers,
     background shells) may survive. Fix: assign each child to a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
2. **`inside()` is lexical only (low, unverified).** A planted junction/symlink named `_manager` could redirect a move
   outside the source dir. Names ending in `.`/space are normalised by Win32. Fix: canonicalize the source dir and the
   destination parent before moving; reject names ending in dot or space.
3. **Move is not atomic no-replace.** `move_to` checks the destination then `rename`s (replace semantics). The new
   "recently written" guard removes the practical race with a running `claude`; a strict fix needs `MoveFileExW`
   without `REPLACE_EXISTING` / `renameat2(RENAME_NOREPLACE)`.
4. **Token in the WebSocket URL (`?t=`).** Not logged by the server, but visible in browser devtools. Acceptable for a
   local app; switch to a WS subprotocol if this ever becomes network-exposed.
5. **No CSP.** If added, xterm.js needs `style-src 'unsafe-inline'`.
6. **ConPTY `ESC[6n` on re-attach.** The UI drops terminal-generated replies while replaying scrollback; a cleaner fix is
   an explicit end-of-replay marker from the server.
7. **Pricing editor freezes default aliases (M1, low).** The UI sends every alias it shows (including the snapshot's
   defaults) as an override on save; if a later snapshot changes a default alias, the old value keeps winning. Fix: send
   only aliases that differ from the defaults (or have GET return defaults and overrides separately).
8. **Cost is an API-list-price estimate.** Subscription plans are not billed per token; treat `costUsd` as "what this
   usage would cost at API prices". Web-search/tool fees, advisor iterations and the 1.1x regional multiplier are not
   included. Prices come from a LiteLLM snapshot (embedded); refresh = rebuild with a new `pricing_snapshot.json`.
9. **Claude Code's log cleanup decides by an unverified rule** (assumed file mtime, `cleanupPeriodDays` default 30).
   `/api/expiring` (and the banner) are warnings, not guarantees. Real data on 2026-11-17: 158 sessions within 5 days.
10. **Block/frozen-history limits.** Frozen (deleted-file) data feeds the heatmap but not the 5-hour blocks; long-context
    tier is exact for frozen rows only with 100k/200k thresholds.
11. **Viewer cold start on huge sessions (M2).** The first `/turns` request of a session builds a byte-offset index of the
    whole file: several seconds for a session of several hundred MB read over `\\wsl$` (9P), then 30-300 ms per page. A progress hint in the UI
    would help; a persistent offset index would remove the wait. Pages of huge sessions can be MBs (tool results are
    capped at 20k chars each); the UI asks for 30 turns per page.
12. **Search limits.** Matching is case-insensitive but diacritic-exact ("tieng" does not find "tiếng"); subagent
    transcripts are not indexed; `total` stops counting at 10 000; queries under 3 chars use a bounded LIKE fallback.
13. **Live refresh costs.** A changed file is re-read in full (a large session can be several hundred MB; re-reads are spaced by cost).
    Polled sources (`\\wsl$` has no change events) do a full scan every ~15 s, so `scan.running` blinks on briefly.
14. **A tool result that lands on a different viewer page than its call shows as an orphan** until the next page loads.
15. **Export** stops at 64 MB and sets `truncated`; HTML export is not implemented.

## Notes
- Opening the app: always via `start.cmd` (or the `Open: http://127.0.0.1:<port>/?k=<token>` line printed on start). The
  token is random per run; an old bookmark shows the "locked" page until you use the new login link.
- Cookie scope: `cm_auth` is HttpOnly + SameSite=Strict and only authorises read-only `/api/*`; writes, terminals and
  WebSockets always need the `X-CM-Token` header / `?t=` that the page JS reads from its meta tag.
