# claude-manager - consolidated feature list and roadmap

Legend: ✅ in current build · 🔨 being built · ⬜ not done · P1/P2/P3 = priority · ❓ = needs a decision · ✖ = rejected (reason)
Sources: [V]=claude-code-viewer [H]=history-viewer [O]=opcode [D]=claude-devtools [K]=klovi [U]=ccusage [N]=our own idea

## Milestone 0 - Parity + Workspace (DONE 2026-11-16/17; verified on the Windows build)
- ✅ Sessions table across Windows + WSL: filter/search/sort, bulk archive/trash/restore/purge, detail drawer with messages
- ✅ Usage tab: tokens by day / project / model / source (numbers identical to the Node prototype on the same set of sessions)
- ✅ Workspace: parallel terminal panes running `claude` per folder (Windows ConPTY, WSL via `wsl.exe`), layouts, resume from sessions, persistent across reload (verified with real ConPTY + WSL + real `claude` 2.1.325)
- ✅ Security: token + Host + Origin, loopback only; after the independent review: cookie login flow (token never in a public page; PoC from the review verified blocked), resume-id flag hardening, live-session guard, bounded parser. Open items in [`KNOWN-ISSUES.md`](KNOWN-ISSUES.md)
- ✅ Rust single binary (about 4 MB), parallel parse (cold scan of a few hundred sessions in seconds over `\\wsl$`, warm about ms), binary cache
- Run: `start.cmd` · Build: `build.cmd` · Tests: `cargo test` (39 pass, run in WSL)

## Milestone 1 - Correct numbers and fast data layer (P1)
**M1a DONE 2026-11-17 (items 1-5, spec in [`M1-SPEC.md`](../specs/M1-SPEC.md); integrated + checked on real data: 237 sessions, `/api/usage` about 40 ms):**
1. ✅ Usage correctness: global dedupe (message.id+requestId, per-field max), sidechain/subagent files read, Billing vs Conversation toggle [U][H][D]
2. ✅ Cost estimation: pricing table from a LiteLLM snapshot (5m/1h cache, fast tier, long-context whole-request), user overrides/aliases, "unpriced" flag, editor in the UI. Not done: background refresh of the snapshot, advisor iterations, web-search fees, regional 1.1x multiplier [U][H]
3. ✅ Local-timezone day buckets; hour x weekday heatmap; tool/skill/subagent stats; active-time metric [U][H]
4. ✅ 5-hour blocks, burn rate, projection. Not done: token-limit warnings, reset-time parse from the usage-limit message [U]
5. ✅ Frozen history (`history.bin`) keeps totals after Claude deletes logs; "expiring soon" banner (`cleanupPeriodDays`) with an "Archive all" button. Archive protects sessions because Claude's cleanup only scans `projects/` [H][V][N]

**M1b + most of M2 DONE 2026-11-18** (spec: [`M1B-SPEC.md`](../specs/M1B-SPEC.md); ported MIT code is listed in [`THIRD-PARTY.md`](../../THIRD-PARTY.md); checked on real data: 237 sessions, search 6-14 ms, 81 tests pass):
6. ✅ Incremental index + SQLite FTS5 full-text search (trigram, Vietnamese works, snippets with highlight offsets, filters). Not done: diacritic-insensitive match, subagent transcripts not indexed
7. ✅ Real project path + WSL/Windows normalisation (`projectKey`), worktree grouping (`repoRoot`), group-by-project in the UI. Not done: project aliases, hide/pin projects
8. ✅ Titles: custom rename (sidecar) > ai-title > summary > first real prompt. Not done: "unify same-title" option, hide-empty is a filter only
9. ✅ Continuation chains via `compact_boundary.logicalParentUuid` (`continuesFrom` / `continuedBy`; none exist in the real data yet, proven by tests). Not done: compaction markers with pre/post token deltas
10. ✅ Live refresh: notify watcher + 15 s polling for `\\wsl$`, `/api/changes` gen, "running" chip

## Milestone 2 - Viewer and session management (P1/P2)
11. ✅ Turn builder + tool kinds, tool cards (summary, visual/raw), real line diff for Edit/Write (`similar` / structuredPatch). Not done: ANSI colours in Bash output
12. ✅ Subagent click-through (`?agent=`). Not done: fold subagent cost into the parent session row
13. ✅ Prompts-only filter, Alt+Up/Down prompt jump, in-viewer find, paging. Not done: minimap/TOC, true virtualised list
14. ✅ Meta rows (compact_boundary, api_error, turn_duration, stop hook, interrupted) and parse-error rows
15. ✅ Tags, stars, notes, rename in the sidecar `meta.json` (never inside Claude's files)
16. ✅ Export Markdown / JSON. Not done: HTML
17. ✅ Global search palette Ctrl+K with source / role / date filters. Not done: filter by tool calls / errors
18. ⬜ Session list: date groups, pin, plan/implementation linking by slug
19. ⬜ Edited-files list per session, todos overlay

## Milestone 3 - Workspace power features (P1/P2)
20. Terminal reliability: seq-numbered ring buffer + `sync{lastSeq}` replay, idle TTL, remembers last session per folder [V]
21. `claude` binary discovery UI (list candidates + version, choose, per WSL distro) [O]
22. Status chips on panes: running/idle/needs-input, git branch + dirty count, token use of the pane's session, context % [D][U][N]
23. Notifications: pane finished, tool error, regex on tool input, per-tool token threshold, rate-limit hit; throttle + snooze + history [D][V]
24. Saved workspaces (a named set of folders + layout opened together), recent folders, queued prompts per pane (send next when idle) [O][N]
25. Copy-as-CLI / resume command builder, open folder in VS Code / Explorer [V][H][N]
26. Statusline command: `claude-manager statusline` prints model, session $, day $, block time left, context % (reads hook JSON on stdin) [U]
27. Headless CLI: `claude-manager usage --json`, `export <id>`, `list` for scripts [U][H]

## Decisions (confirmed by the user)
- Config manager: edits allowed, ALWAYS a timestamped backup + diff preview before save + restore button; hooks/permissions get explicit warnings.
- Rename/tags/notes: stored in claude-manager's own sidecar store; Claude's jsonl is never rewritten. (Later amended for rename: the app may append a `custom-title` line to an idle session file.)
- Rate-limit auto-continue: OFF by default, opt-in per pane, with countdown + cancel; default behaviour is notify only.
- Scope: Claude Code only (provider abstraction kept so another provider can be added later).

## Milestone 4 - Config manager (P2) ✅ approved, writes to Claude's own config
28. Browse + edit with backup/atomic write: CLAUDE.md files (global/project), settings.json (permissions, env, model), hooks (user/project/local merge), slash commands / skills / agents (frontmatter), plugins list, MCP servers (read `.claude.json` / `.mcp.json` directly, no scraping `claude mcp list`) [O][V]
29. Light git panel per folder: branch, status, diff (numstat + unified), no destructive ops in v1 [V][N]

## Later (P3)
- Per-turn context breakdown (CLAUDE.md/files/tool output/thinking) as a stacked bar [D]
- Presentation/replay mode for a session [K]
- Themes, font size, configurable hotkeys, i18n (vi/en) [K][V][H]
- Remote filesystem providers (SSH) using the same source abstraction as WSL [D]
- Other tools' histories (Codex etc.) via provider plugins [K][H][U] - decided: not now (Claude Code only)

## Rejected (and why)
- ✖ Chat through the Agent SDK / `claude -p --dangerously-skip-permissions` [V][O]: ToS risk, permission bypass; we run the real CLI in a PTY instead.
- ✖ Checkpoint/timeline restore that deletes non-snapshot files, and "restore edits from history" [O][H]: destructive, repeated safety bugs.
- ✖ Rewriting Claude's own jsonl for rename [H]: can lose a concurrent append, so use a sidecar (or append-only, see ❓).
- ✖ Writing caches into `~/.claude/projects` [H]; web mode on 0.0.0.0 / no auth [O][K]; posthog telemetry [O]; auto-updater, PWA/web-push, Argon2 login UI, SQLite browser, Mermaid, team UI, Session Board [H][D][V].
- ✖ Heuristic token "estimates" (chars/4) for anything billing-related [D]: display-only at most.
