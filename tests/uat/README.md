# Automated UAT

`settings-uat.js`: tests the Settings dialog against the real exe plus headless Edge (CDP). Requires Node 24
(built-in `WebSocket`, no `npm install` needed) and Microsoft Edge.

```powershell
node tests\uat\settings-uat.js [path\to\claude-manager.exe]
```

By default it uses `bin\claude-manager.exe` (or the `CM_EXE` environment variable). The script runs the exe on port 8796
with `CM_HOME` set to a temporary directory (a copy of `bin\config.json`), changes every setting in turn through the UI,
saves, reloads, checks that the stored values and the displayed UI are correct, and then restores the defaults. It exits
with a non-zero code on failure and only kills the Edge/exe processes it started itself.
