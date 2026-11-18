# web - frontend

Plain HTML/CSS/JS with no framework and no build step. The whole folder is embedded into the exe (rust-embed) and served
by `src/web.rs`. All untrusted strings (paths, titles, model names) go through `esc()` before reaching `innerHTML`.
Shared helpers are exposed on `window.CM` by `app.js`.

| File | Purpose |
|---|---|
| `index.html` | Page shell: tabs (Sessions, Usage, Workspace, Inventory), dialogs, script loading order |
| `app.js` | Shared helpers (`window.CM`), Sessions tab, Usage tab wiring, tab switching, auto refresh |
| `usage.js` | Usage tab: cost, 5-hour blocks, heatmap, tools, pricing editor (`/api/usage`, `/api/blocks`, `/api/pricing`) |
| `workspace.js` | Workspace tab: xterm.js panes over WebSocket PTYs, layouts, resume, persisted via `/api/workspace` |
| `viewer.js` | Session viewer drawer (`/api/session/{id}/turns`, export) |
| `search.js` | Ctrl/Cmd+K full-text search palette (`/api/search`) |
| `settings.js` | Settings dialog and per-tab Help panel (`/api/settings`) |
| `inventory.js` | Inventory tab: skills / plugins / MCP / hooks / CLAUDE.md and spec-kit dashboard |
| `i18n.js` | Translation loader and `t()` helper; loads `locales/<lang>.json` before the other scripts |
| `theme.js` | Applies the saved light/dark override before first paint |
| `style.css` | All styles (light/dark, responsive down to 390 px) |
| `favicon.svg` | Icon |
| `vendor/` | xterm.js, fit addon, xterm.css, `LICENSE-xterm` (vendored for offline use) |
| `locales/` | UI strings: `vi.json`, `en.json` ([`locales/README.md`](locales/README.md)) |

Backend routes used by each file are listed in [`../src/README.md`](../src/README.md).
