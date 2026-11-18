# tests

- **Rust unit/integration tests**: inline `#[cfg(test)]` modules in [`../src`](../src/README.md). Run in WSL:
  `cargo test` (rsync a private copy first if the Windows target dir is busy). The count grows with each milestone.
- **UI acceptance tests**: [`uat/`](uat/README.md) - `settings-uat.js` drives the real exe with headless Edge (CDP) and checks
  every setting end to end. Needs Node 24 and Microsoft Edge.

Test servers must never use port 8765 (the real instance); the UAT uses 8796.
