@echo off
rem Release build -> bin\claude-manager.exe. Needs Rust (rustup) + Visual Studio Build Tools (C++).
rem Stop a running claude-manager first (Windows locks the exe).
setlocal
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
set "CARGO_TARGET_DIR=%LOCALAPPDATA%\cm-target"
cd /d "%~dp0"
cargo build --release || exit /b 1
if not exist bin mkdir bin
copy /y "%CARGO_TARGET_DIR%\release\claude-manager.exe" bin\claude-manager.exe >nul && echo Built: %~dp0bin\claude-manager.exe
