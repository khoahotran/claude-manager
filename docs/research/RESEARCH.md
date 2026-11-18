# Research digest (6 reference projects, ideas only - no code copied)

Reviewed by reading source (shallow clones, deleted afterwards): claude-code-viewer (MIT), claude-code-history-viewer (MIT),
opcode (AGPL - ideas only), claude-devtools (MIT), klovi (MIT), ccusage (MIT).

None of them has: Windows+WSL merged view, archive/trash/restore with restore, daily/project/model rollups from raw usage
(except ccusage CLI), or parallel terminal panes with Windows ConPTY. Those are our differentiators.

## Parser rules to implement (collected from all six)
Layout / identity
- `projects/<encoded-cwd>/<uuid>.jsonl`; subagents at `<uuid>/subagents/agent-<id>.jsonl` (+ `agent-*.meta.json` with `toolUseId`),
  legacy at project root `agent-*.jsonl` (match first-line sessionId). Skip subagent files whose first user text is exactly "Warmup" or agentId starts "acompact".
- Folder-name encoding is lossy. Real path = `cwd` from the first entries (try several files, tolerate null). Fast path: `sessions-index.json` `originalPath`.
  Normalise drive-letter case; map `/mnt/c/..` <-> `C:\..` and `\\wsl$\..` so WSL and Windows copies of one repo can be grouped.
- Group worktrees into the main repo (`.git` file with `gitdir:` pointer).
- Continuation chain: a new file whose `system/compact_boundary.logicalParentUuid` is absent from its own uuids continues the file containing that uuid
  (max 50 hops). Hide superseded predecessors; a file still receiving main-thread turns after the branch point is a real fork.

Line handling
- Tolerate blank/invalid/truncated-last lines; never abort; unknown `type` kept raw (open set: user, assistant, system, summary, progress, queue-operation,
  file-history-snapshot, custom-title, ai-title, agent-name, permission-mode, pr-link, last-prompt, attachment ...). Show unparseable lines as "parse error" rows with line number.
- Skip noise by byte-prefix before JSON parse; cap line length; guard against FIFO/symlink/dir named *.jsonl.
- `isMeta` rows, `<local-command-*>`, `<command-name>` family, `<system-reminder>`, `<task-notification>` are not prompts; slash commands render as `/cmd args`;
  `<bash-input>` + `<bash-stdout/stderr>` merge into one terminal-style turn; "[Request interrupted by user" ends activity.

Turns
- Assistant lines + tool_result-only user lines = one logical turn; pair tool_use.id <-> tool_result.tool_use_id (orphans stay visible); duration = result ts - call ts.
- Normalise tool names to kinds (shell, file_read, file_write, file_edit, search, web, subagent, skill, mcp, todo, generic) with a one-line summary.
- Subagent link: `toolUseResult.agentId` (or regex `agentId: X` in result text), then `meta.json toolUseId`, then prompt match.
- Compaction: `isCompactSummary` user entry / `compact_boundary{preTokens}`; show markers with pre/post tokens.

Titles: custom-title (rename) > ai-title > summary > first genuine user text (skip caveat/Warmup/commands) > "/cmd args" > session id.
Status: "running" = activity after last ending event and file mtime < 5 min.

## Usage / cost rules (ccusage + history-viewer + devtools)
- Dedupe BEFORE bucketing, globally across files and sources. Key (message.id, requestId); within a key take per-field max (output_tokens grows while streaming).
  No requestId + `msg_`/`resp_` id: same id+session. Sidechain replays (isSidechain repeating parent message ids) dropped. Entries without id never deduped.
- Skip model `<synthetic>`. Subagent usage counts toward parent session. Offer Billing (incl. sidechains) vs Conversation-only.
- total = input + output + cache_read + cache_creation. Cache write split: `usage.cache_creation.ephemeral_5m/1h_input_tokens` (1h priced higher).
  `speed:"fast"` / `service_tier:"priority"` multiplier. Long-context tier is whole-request (input+cache_read+cache_creation of ONE request vs threshold).
  Advisor iterations (`usage.iterations[type=advisor_message]`) priced under advisor model.
- Prices live in data (embedded snapshot + optional refresh with ETag/timeout/validation + user overrides + model aliases). Unknown model => "unpriced", never $0 silently.
- Bucket days in a user-chosen validated timezone (reject unknown names). 5-hour billing blocks use UTC-floored hour start; active if now < end.
- Burn rate = tokens/min over block; projection to block end; limit warn at 80% / exceed at 100%. Usage-limit text "...limit reached|<epoch>" gives reset time.
- Claude Code deletes transcripts after `cleanupPeriodDays` (default 30) => archive + persist daily totals ourselves.

## Process / terminal lessons (opcode, claude-code-viewer)
- Find `claude`: stored path, PATH (`where`), nvm/npm-global/`~/.claude/local`/`.local/bin`/bun, VS Code extension native binary; run `--version` on candidates; UI to choose; per WSL distro.
- Windows: `.cmd` shim needs `cmd /c` or run exe; kill with `taskkill /T /F` (process tree). Whitelist env or inherit carefully.
- Terminal reconnect: seq-numbered ring buffer + `sync{lastSeq}` replay, idle TTL, max terminals, remember last session per cwd.
- `--resume` may create a NEW session id; follow `system/init` / newest file in the folder.

## Security lessons (what the others got wrong)
klovi: no auth/Host/Origin checks. opcode web mode: 0.0.0.0 + CORS any + no auth + skip-permissions = LAN RCE. viewer: `startsWith` path guard (prefix-confusable).
history-viewer: rewrites Claude's own jsonl and writes caches into `~/.claude/projects`. => We: loopback only, token + Host + Origin, JSON-only POST,
safe path joins (component-wise), never mutate Claude's jsonl, own cache/sidecar dir, symlink/FIFO guards.
