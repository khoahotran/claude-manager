# Locale files

`vi.json` (source of truth, current Vietnamese text) and `en.json` (English). Both have exactly the same key set (777 keys). Wired into the UI by `web/i18n.js`.

## Format

- Nested JSON, grouped by area: `common`, `time`, `header`, `api`, `sessions`, `drawer`, `usage`, `pricing`, `workspace`, `viewer`, `search`, `settings`, `help`, `inventory`, `toast`.
- Look-up by dotted path: `t('sessions.empty')`, `t('help.usage.calc_3', {tz})`.
- Keys are lowercase `snake_case`. Suffixes: `_tip` = title/tooltip, `_label` = aria-label, `_ph` = placeholder, `_hint` = helper text, `_html` = value contains trusted HTML.
- `toast.*` holds toasts that are shown from code (`toast.sessions_vscode_failed`, `toast.copied`, ...). Confirm-dialog texts stay with their area (`sessions.confirm_*`, `workspace.confirm_*`).

## Placeholders

- `{name}` interpolation, e.g. `"{n} session sắp bị Claude Code xoá trong {when}{urgent}"`. vi and en must use the same placeholder names (the check script verifies this).
- `t()` does not escape. Values that go into `innerHTML` must be passed through `esc()` by the caller. Strings under `help.*`, `settings.*_html` and `*_html` contain trusted markup (`<b> <u> <code> <kbd>`), use them only with innerHTML.
- Leading/trailing spaces in a few values (e.g. `" · {n} đã kết thúc"`, `" (lỗi)"`) are intentional: they are appended fragments. Keep them.

## Plurals

- Plural keys come in pairs `<key>_one` / `<key>_other`; `t(key, {n})` picks by `Intl.PluralRules(lang).select(n)` and falls back to `<key>_other`, then `<key>`.
- vi has both keys with identical text (Vietnamese has no plural) so the key sets stay identical.
- Only keys where English really differs got a distinct `_one`; other counted strings (e.g. `{n} session(s)` in confirms) use the "(s)" form to avoid a pair.
- Do not name non-plural keys `*_one` / `*_other` (`select_row`, `star_row` instead).

## How the UI will load them

Plan: a new `i18n.js` loaded before `app.js`:

- `t(key, params)`: look up in the current language, fall back to `vi`, then to the key itself; replace `{name}`; plural as above.
- Language comes from Settings `ui.lang` = `"vi"` | `"en"` (default `"vi"`), stored with the other `ui` settings (server `/api/settings` + `cm-ui` in localStorage). New keys already prepared: `settings.lang_*`.
- Locale files are served as static assets (`web/locales/*.json`, embedded by rust-embed); load the active one (and `vi` for fallback) before the first render, then set `<html lang>`.
- Static HTML (`index.html`) gets `data-i18n="key"`, `data-i18n-title`, `data-i18n-aria-label`, `data-i18n-placeholder` attributes applied on load and on language change.
- Dates/numbers: `time.locale` holds the BCP-47 tag for `toLocaleString` / `toLocaleDateString` (`vi-VN` / `en-US`). Weekday abbreviations are `usage.weekday_*`.

## Unsure / needs a decision

| Key / string | Note |
| --- | --- |
| `header.tab_inventory` ("Kho") | en "Library"; the code and tooltip call it "inventory". Could be "Inventory". |
| `sessions.status_trashed`, `sessions.bulk_trash` | "Thùng rác" = "Trash"; the action verb is "Move to trash". |
| `sessions.filter_star` | vi "Đã star" keeps the English loanword; en "Starred". |
| `time.*` relative words | Code uses 30 days as the cut-off to a locale date; that logic stays in JS. `time.in_days` / `next_days` are used by the expiry banner only. |
| `sessions.exp_banner_*` | `{when}` and `{urgent}` are filled from `time.today` / `time.tomorrow` / `time.in_days_*` / `time.next_days_*` (vi: "trong {when}"; en: "deleted by Claude Code {when}" -> "in 5 days", "today", "within 5 days"). `{urgent}` = `sessions.exp_urgent` with its own `{when}`. |
| `usage.heat_subtitle_tz` | `{tz}` is "UTC+7" or `usage.heat_tz_local`; vi reads "giờ UTC+7" / "giờ địa phương", en "UTC+7 time" / "local time". |
| `usage.unit_min_axis` | vi "p", en "m" on the active-time chart axis. |
| `usage.weekday_*` | vi T2..T7/CN; en Mon..Sun. |
| `usage.metric_*`, `usage.mode_*` | "Billing"/"Conversation" are technical terms, not translated. |
| `search.input_ph` | vi has "(tiếng Việt OK)", dropped in en (it says the search handles Vietnamese diacritics; en has no equivalent). |
| `sessions.exp_*`, `viewer.*` Claude Code terms | `compact`, `/clear`, `sidechain`, `tool_result`, `effort`, `skill`, `hook` kept untranslated in both. |
| `settings.src_dir_ph` | vi placeholder path `C:\Users\bạn\.claude`, en `C:\Users\you\.claude`. |
| `help.*` | Long help paragraphs: en is a faithful rewrite, not literal; review the tone. |
| `viewer.dur_min_sec` | Used only for durations of 1 minute or more; the seconds-only forms (`1,5 s`, `850 ms`) are numeric and have no key (vi uses a decimal comma, en should use a dot: needs `fmtDur` change in code). |
| Legacy message list roles | `user` / `assistant` / `tool` / `result` / `meta` role names are printed raw by `app.js`; no keys created. |
| Session titles `(empty session)` | Fallback title already English in code; kept as `common.empty_session` in both. |
| `inventory.warn_over_200_lines` | Server sends the English text "Over 200 lines" in `warn[]`; the UI maps it to this key. |
| Item rows with raw API values | `status` (`active`/`archived`/`trashed`) badge in the drawer, source names (`Windows`, WSL distro), model ids: shown raw, not translated. |

## Server strings

Backend console messages, action errors and the inventory warning are English. The locked page (`src/web.rs` `LOCKED_HTML`) is bilingual (Vietnamese and English).
