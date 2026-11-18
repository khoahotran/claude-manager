@echo off
rem Starts claude-manager (config.json / cache.bin / workspace.json live next to the exe in bin\).
rem If it is already running this just reopens the browser on the running instance.
cd /d "%~dp0bin"
if not exist claude-manager.exe (
  echo claude-manager.exe not found in %CD%. Run build.cmd first.
  pause
  exit /b 1
)
claude-manager.exe %*
if errorlevel 1 (
  echo.
  echo claude-manager exited with an error ^(see the message above^).
  pause
)
