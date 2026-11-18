# Publish checklist

## GitHub settings checklist
Replace the `khoahotran/claude-manager` placeholders in README.md, SECURITY.md, CHANGELOG.md and
`.github/ISSUE_TEMPLATE/config.yml` first. LICENSE (MIT) and `license = "MIT"` are already in place.

- [ ] Add `repository = "https://github.com/<owner>/<repo>"` (the real URL) to `[package]` in Cargo.toml once the repo exists
- [ ] Confirm the repo URLs in THIRD-PARTY.md still point at the original projects

- [ ] Branch protection (ruleset) for `main`: require a pull request, require the CI checks (`rust (windows-latest)`,
      `rust (ubuntu-latest)`, `web (node checks)`) to pass, require branches to be up to date
- [ ] Block force pushes and branch deletion on `main`
- [ ] Settings > Code security: enable secret scanning and push protection
- [ ] Enable Dependabot alerts and security updates (version updates come from `.github/dependabot.yml`)
- [ ] Enable private vulnerability reporting (needed by SECURITY.md)
- [ ] Settings > Actions > General: default workflow permissions "Read repository contents"; the release workflow
      raises `contents: write` only for its own job
- [ ] About box: description, website (if any) and topics (for example `rust`, `claude-code`, `axum`, `wsl`,
      `session-manager`)
- [ ] Social preview image (1280x640), made with demo data only
- [ ] README screenshots taken with demo data (no real session titles, paths or tokens)
- [ ] Create the first tag `v0.1.0` to exercise the release workflow
