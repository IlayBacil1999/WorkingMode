@echo off
rem Builds working-mode.exe and copies it next to this script.
rem A short target dir avoids the 260-char path limit of the MSVC linker.
cd /d "%~dp0"
set CARGO_TARGET_DIR=%TEMP%\wm-target
cargo test --release || exit /b 1
cargo build --release || exit /b 1
copy /y "%CARGO_TARGET_DIR%\release\working-mode.exe" "%~dp0working-mode.exe" >nul
echo Built %~dp0working-mode.exe
