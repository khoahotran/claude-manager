# Changelog

All notable changes are documented here. Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning: [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.0]

### Added
- Sessions table across Windows and WSL: filter, sort, group by project, star, tag, note, rename; bulk archive, trash,
  restore and purge; session viewer with tool cards, diffs, subagent click-through and Markdown/JSON export.
- Global full-text search (Ctrl+K) on SQLite FTS5, including Vietnamese text.
- Usage: tokens and estimated cost by day, project, model and source; 5-hour billing blocks; heatmap; tool, skill and
  subagent stats; frozen history that survives Claude Code log cleanup; editable pricing table.
- Workspace: parallel real `claude` terminals (Windows ConPTY, WSL via `wsl.exe`), resume from a session, panes survive
  page reloads.
- Inventory: skills, plugins, MCP servers, hooks, CLAUDE.md sizes, spec-kit progress.
- Settings, light/dark theme, Vietnamese and English UI.
- Security: loopback only, per-run token with cookie login, Host and Origin checks.
- Single Rust binary with the UI embedded.

[Unreleased]: https://github.com/khoahotran/claude-manager/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/khoahotran/claude-manager/releases/tag/v0.1.0
