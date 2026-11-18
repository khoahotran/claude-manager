# Milestone 1 spec - correct usage numbers, cost, blocks, retention

Context: [`FEATURES.md`](../planning/FEATURES.md) (M1 items 1-5), [`RESEARCH.md`](../research/RESEARCH.md) (rules distilled from ccusage / history-viewer / claude-devtools),
[`DESIGN.md`](../architecture/DESIGN.md) (architecture + security). Everything below is a CONTRACT between the backend and the web UI.

## Backend changes (Rust)
1. **Event-level usage in the cache.** For every session file keep compact usage events (not only daily sums):
   per assistant message: `key` (hash of message.id + requestId), ts (ms), model, in, out, cc5, cc1, cr, flags
   (sidechain, speed fast, requestId-missing), plus per-file tool counters {tool name -> n}, skill names (Skill tool
   `input.skill`), subagent types (Task/Agent `input.subagent_type`). Bump CACHE_VERSION. The per-session `Summary`
   fields (`tok`, `daily`, ...) stay (same JSON shape) but are computed with the new dedupe below.
2. **Dedupe** (ccusage/history-viewer rules): collapse by (message.id, requestId); within a key take the per-field MAX
   (output_tokens grows while streaming; input/cache repeat). No requestId + `msg_`/`resp_` id => same id + same session.
   Sidechain replays (isSidechain lines repeating a parent message id) are dropped when the parent copy exists.
   Entries without message.id are never deduped. Skip model `<synthetic>`. Dedupe is GLOBAL (across files and sources) and
   happens at aggregation time, before bucketing.
3. **Cache write split:** `usage.cache_creation.ephemeral_5m_input_tokens` -> cc5, `...1h...` -> cc1; if only the flat
   `cache_creation_input_tokens` exists, treat all as cc5. `usage.speed=="fast"` or `service_tier=="priority"` => fast flag.
4. **Pricing** (`src/pricing.rs`). Default table embedded in the binary from a LiteLLM snapshot (see "Pricing data"), user
   overrides in `<app_dir>/pricing.override.json`. Lookup: exact id, alias, then boundary-aware longest-prefix (strip an
   8-digit date suffix and a `[1m]` suffix; `.` `@` `-` equivalent). Cost = in*inRate + out*outRate + cc5*cacheWrite5m +
   cc1*cacheWrite1h + cr*cacheRead; missing cache rates default to 1.25x / 2x / 0.1x of the input rate ONLY when the entry
   says so (`derived:true`); fast multiplier when `fastMultiplier` is set; long-context tier is WHOLE-REQUEST: if
   (in + cr + cc5 + cc1) of one request > threshold, the tier rates apply to that whole request. Unknown model => cost
   `null`, `priced:false` (never $0). Do not invent prices: only data from the snapshot or the user's overrides.
5. **Aggregation endpoints** (below) computed from events, in the machine's local timezone (chrono Local; document it).
6. **Retention / frozen history.** Claude Code deletes transcripts after `cleanupPeriodDays` (read from
   `<claudeDir>/settings.json`, default 30). When a file vanishes from disk and was NOT moved by our own
   archive/trash/restore, keep its contribution: convert its events to per-(day, model, project, source) aggregates and
   keep them in a `history.bin` (atomic write) so totals do not drop. Archived and trashed files still count (tokens were
   spent). Mark such rows in the response (`retention.frozenDays`). Archiving also protects from deletion (the cleanup
   only looks at `projects/`): `/api/expiring` lists sessions about to expire so the UI can offer "archive them".
7. Keep every existing endpoint and field working. New endpoints are read-only GETs except PUT /api/pricing (strict,
   token + JSON, size <= 256 KiB, validated).

## Endpoints (JSON; camelCase)
`GET /api/usage?from=YYYY-MM-DD&to=YYYY-MM-DD&mode=billing|conversation` (defaults: last 30 days, billing;
`conversation` excludes sidechain/subagent events):
```jsonc
{ "generatedAtMs": 0, "tz": "+07:00", "mode": "billing",
  "rows": [ { "day":"2026-11-15", "source":"WSL Ubuntu", "project":"/home/u/proj", "model":"claude-sonnet-5-5",
              "requests": 12, "in":0, "out":0, "cc5":0, "cc1":0, "cr":0, "costUsd": 1.23 | null, "priced": true } ],
  "totals": { "requests":0, "in":0, "out":0, "cc5":0, "cc1":0, "cr":0, "costUsd":0.0, "unpricedTokens":0 },
  "unpricedModels": ["model-x"],
  "heat": [[0 x 24] x 7],               // weekday 0=Mon..6=Sun, hour 0..23 local; value = in+out tokens
  "tools": [ {"name":"Bash","count":10} ],  "skills": [ {"name":"x","count":1} ],  "subagents": [ {"name":"Explore","count":2} ],
  "activeMinutesByDay": { "2026-11-15": 95 },   // sum of gaps <= 120 min between consecutive events
  "retention": { "cleanupPeriodDays":30, "oldestLiveDay":"2026-10-17", "frozenDays": 4 } }
```
`GET /api/blocks`

```jsonc
{ "blocks": [ { "startMs": 0, "endMs": 0, "lastEntryMs": 0, "isActive": true, "isGap": false,
                "requests": 0, "in": 0, "out": 0, "cc5": 0, "cc1": 0, "cr": 0, "costUsd": 1.2 | null, "models": ["..."],
                "burn": { "tokensPerMin": 0, "tokensPerMinNoCache": 0, "costPerHour": 0.0 } | null,
                "projected": { "tokens": 0, "costUsd": 0.0 | null, "remainingMinutes": 0 } | null } ] }
```

5-hour billing blocks (start = first entry floored to the UTC hour; new block when an entry is >5h after the block start or >5h after the previous entry;
gap blocks for idle time; active if now < end and last entry < 5h old; burn/projected only for the active block).
Covers the last 30 days; ascending. (Later changed: the start is floored to the minute.)

`GET /api/pricing`

```jsonc
{ "snapshotDate": "2026-11-17", "snapshotSource": "litellm",
  "models": [ { "id": "claude-sonnet-5-5", "match": "prefix",
                "inPerM": 0, "outPerM": 0, "cacheReadPerM": 0, "cacheWrite5mPerM": 0, "cacheWrite1hPerM": 0,
                "longContext": { "thresholdTokens": 200000, "inPerM": 0, "outPerM": 0, "cacheReadPerM": 0, "cacheWrite5mPerM": 0, "cacheWrite1hPerM": 0 } | null,
                "fastMultiplier": 6 | null, "source": "litellm|override", "derived": false } ],
  "aliases": { "alias": "target" },
  "seenModels": [ { "model": "x", "priced": true, "tokens": 0 } ] }
```

`PUT /api/pricing`, body:

```jsonc
{ "models": [ ...override entries, same shape... ], "aliases": { ... } }
```

Response: same as GET (overrides win).

`GET /api/expiring`

```jsonc
{ "cleanupPeriodDays": 30, "warnDays": 7,
  "sessions": [ { "id": "..", "title": "..", "source": "..", "cwd": "..", "lastMs": 0, "daysLeft": 3 } ] }
```

Lists ACTIVE sessions whose file age >= cleanupPeriodDays - warnDays (per source).

## Pricing data
The agent downloads LiteLLM's `model_prices_and_context_window.json`
(https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json), keeps only Claude entries
(keys starting with `claude` or containing `/claude`), and writes `src/pricing_snapshot.json` (small, committed into the
repo, embedded with include_str!) with `snapshotDate`. LiteLLM per-token prices -> per-million. Models in the user's data
that are not in the snapshot stay UNPRICED (shown as such in the UI; the user can add an override). No hand-typed prices.

## Web UI changes (web/ only)
Usage tab: add cost everywhere (totals cards, chart metric "Cost (USD)", tables by project/model/source/day) driven by
/api/usage instead of the per-session `daily`; range presets 7/14/30/90 days/all + custom from-to; mode toggle
Billing / Conversation; warning banner listing `unpricedModels` with a button opening a small pricing editor (reads
/api/pricing, saves overrides via PUT); 5-hour blocks panel (active block with progress bar, burn rate, projection;
list of recent blocks); hour x weekday heatmap; top tools / skills / subagents; active-time per day; retention note
(frozen days, oldest live day). Sessions tab: banner when `/api/expiring` is non-empty: "N sessions will be deleted by
Claude Code within X days" + button "Archive all" (uses POST /api/action archive) and a per-row badge. Keep the UI
translatable (the original UI was Vietnamese; see `web/locales/`), light/dark, responsive, accessible. Costs display as USD with
2-4 decimals and an "estimate" hint; unpriced shows "no price yet", never $0.00.

## Acceptance
Backend: unit tests for dedupe (streaming growth, requestId rules, sidechain replay, synthetic), cost (5m/1h split, fast,
long-context whole-request, unknown model => null), blocks (boundaries, gap, active), retention (vanished file keeps
contribution; archive move does not double count), endpoints shape. On the real data report: session count, total tokens
before vs after the new dedupe (explain any difference), `/api/usage` timing cold/warm, models seen but unpriced.
Frontend: screenshots light/dark/390px of Usage + Sessions banner, tested against a mock AND (once available) the real
backend. Both: no regressions in existing endpoints; security rules unchanged (read-only GETs need the cookie or token).
