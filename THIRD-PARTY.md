# Third-party code

Code adapted (ported/translated) from MIT-licensed projects, plus vendored libraries and data. Each entry: our file,
source repo + path, license, copyright. All of these are under the MIT License; its full text is reproduced once at the
end of this file and applies to each copyright line listed in "Copyright notices".

## src/search.rs
- Source: claude-code-viewer - `src/server/core/search/services/SearchService.ts` (FTS5 query escaping, BM25 rank with x1.2
  user boost, snippet window 50/150), `src/server/lib/db/DrizzleService.ts` (FTS5 `trigram` table DDL),
  `src/server/core/sync/services/SyncService.ts` + `src/server/core/search/functions/extractSearchableText.ts`
  (per-message text extraction, delete-then-insert re-index per session). Translated TypeScript -> Rust.
  License: MIT. Copyright (c) 2025 Claude Code Viewer
- Source: claude-code-history-viewer - `src-tauri/src/commands/session/search.rs` ((size, mtime) file signature for incremental
  re-indexing; deleted/new file detection). License: MIT. Copyright (c) 2025 JaeHyeok Lee

## src/index.rs (continuation chain, worktree repoRoot), src/parse.rs (chain/title/live-state fields), src/watch.rs
- Source: claude-code-history-viewer - `src-tauri/src/commands/session/chain.rs` (dangling `compact_boundary.logicalParentUuid`
  -> predecessor file holding that uuid, same project dir; oldest copy wins), `src-tauri/src/commands/watcher.rs` +
  `src-tauri/src/utils.rs::strip_windows_extended_prefix` (debounced notify watcher, (len, mtime) signature filter,
  `\\?\` / `\\?\UNC\` stripping). Rewritten to our index/cache types. License: MIT. Copyright (c) 2025 JaeHyeok Lee
- Source: klovi - `packages/plugin-core/src/resolve-worktree.ts` (`.git` file `gitdir: <main>/.git/worktrees/<name>`
  -> main repo), `packages/plugin-claude-code/src/command-message.ts` + `discovery.ts` (`<command-name>/<command-args>`
  -> "/cmd args", internal-message skip for titles). Translated TypeScript -> Rust.
  License: MIT. Copyright (c) 2025 Cookielab s.r.o.
- Source: claude-devtools - `src/main/utils/sessionStateDetection.ts` (ongoing = AI activity after the last ending
  event: text output / interruption / rejected tool use / ExitPlanMode). Translated TypeScript -> Rust (streaming form).
  License: MIT. Copyright (c) 2026 claude-devtools contributors

## src/viewer.rs
- Source: klovi - `packages/plugin-claude-code/src/parser.ts` (turn building: assistant lines + tool_result-only user
  lines = one turn, skipped user wrappers, `<bash-input>`/`<bash-stdout>`/`<bash-stderr>` merge, `TOOL_NORMALIZERS`
  tool kind + summary table, `agentId:` result regex and `agent_progress` subagent mapping, parse-error rows with
  line number and first 500 chars), `packages/plugin-claude-code/src/command-message.ts` (`<command-name>` /
  `<command-args>` -> "/cmd args"). Translated TypeScript -> Rust, rewritten for byte-range paging.
  License: MIT. Copyright (c) 2025 Cookielab s.r.o.
- Source: claude-code-history-viewer - `src-tauri` message rendering (use `toolUseResult.structuredPatch` hunks for real Edit/Write
  line numbers). License: MIT. Copyright (c) 2025 JaeHyeok Lee
- Source: claude-code-viewer / claude-devtools - subagent linking via `<session>/subagents/agent-<id>.meta.json`
  `toolUseId` + `toolUseResult.agentId`. Ideas only, re-implemented. License: MIT. Copyright (c) 2025 Claude Code Viewer;
  Copyright (c) 2026 claude-devtools contributors

## src/parse.rs, src/usage.rs (usage rules, 5-hour billing blocks)
- Source: ccusage - deduplication and 5-hour block rules. Ideas only, re-implemented; no code copied.
  License: MIT. Copyright (c) 2025 ryoppippi

## web/vendor/ (xterm.js, xterm.css, addon-fit.js)
- Source: xterm.js and its fit addon, vendored unmodified for offline use. License: MIT (full text with all copyright
  lines in [`web/vendor/LICENSE-xterm`](web/vendor/LICENSE-xterm)).

## src/pricing_snapshot.json
- Source: LiteLLM (BerriAI/litellm) - `model_prices_and_context_window.json`, Claude entries and price fields only,
  embedded in the binary. License: MIT. Copyright (c) 2023 Berri AI

## Copyright notices
- claude-code-viewer (https://github.com/d-kimuson/claude-code-viewer): Copyright (c) 2025 Claude Code Viewer
- claude-code-history-viewer (https://github.com/jhlee0409/claude-code-history-viewer): Copyright (c) 2025 JaeHyeok Lee
- claude-devtools (https://github.com/matt1398/claude-devtools): Copyright (c) 2026 claude-devtools contributors
- klovi (https://github.com/cookielab/klovi): Copyright (c) 2025 Cookielab s.r.o.
- ccusage (https://github.com/ryoppippi/ccusage): Copyright (c) 2025 ryoppippi
- xterm.js (https://github.com/xtermjs/xterm.js): Copyright (c) 2017-2019, The xterm.js authors;
  Copyright (c) 2014-2016, SourceLair Private Company; Copyright (c) 2012-2013, Christopher Jeffrey
- LiteLLM (https://github.com/BerriAI/litellm): Copyright (c) 2023 Berri AI

## MIT License (applies to each project above)

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
