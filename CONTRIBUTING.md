# Contributing

Thanks for helping. Small, focused pull requests are easiest to review; open an issue first for larger changes.
Please follow the [Code of Conduct](CODE_OF_CONDUCT.md) and report vulnerabilities as described in
[SECURITY.md](SECURITY.md).

## Development setup (Windows)
1. Install Rust with [rustup](https://rustup.rs) and Visual Studio Build Tools (workload "Desktop development with C++").
2. Optional: WSL with a distro, for Linux-side Claude sessions and for running tests.
3. `build.cmd` builds `bin\claude-manager.exe`; `start.cmd` runs it. Stop a running instance before rebuilding
   (Windows locks the exe). Never use port 8765 for test servers; that is the real instance.
4. The UI is plain JS/CSS in `web/` and is embedded into the exe at build time, so rebuild after UI edits.

Layout and design: [docs/README.md](docs/README.md), [docs/architecture/DESIGN.md](docs/architecture/DESIGN.md).

## Tests
- Rust: `cargo test`, in WSL or on Windows. If the Windows `target` directory is busy, copy the repo to a private
  folder first.
- Web: `for f in web/*.js; do node --check "$f"; done` and `node tests/check-locales.js` (vi and en keys and
  placeholders must match).
- UAT: `tests/uat/settings-uat.js` drives the real exe with headless Edge (Node 24 + Edge); see
  [tests/uat/README.md](tests/uat/README.md).

## Code style
- Run `cargo fmt` before committing (the code is rustfmt-clean). Keep code you touch clippy-clean.
- Comments only for "why", never for "what". Code, comments, docs and commit messages in English.
- No long dashes in text or comments; use a hyphen or rephrase.
- Every user-visible string goes through the locale files, in both `vi.json` and `en.json`.
- Never rewrite Claude's own session files; use the app's sidecar files.
- Never commit real session titles, paths, tokens or screenshots containing them.

## Pull request checklist
- [ ] Tests added or updated; `cargo test` passes
- [ ] `cargo fmt --check` clean; web checks pass
- [ ] Docs updated (`docs/planning/FEATURES.md`, affected READMEs) and a CHANGELOG.md entry under [Unreleased]
- [ ] Security impact considered (auth, Host/Origin, process spawning, file writes)
- [ ] New dependencies and their licenses reviewed; ported code recorded in [THIRD-PARTY.md](THIRD-PARTY.md)
