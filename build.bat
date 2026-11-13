@echo off
rem Сборка mihomo-tray.exe из исходников (нужны Rust и Visual Studio Build Tools с C++).
rem   winget install Rustlang.Rustup
rem   winget install Microsoft.VisualStudio.2022.BuildTools --override "--passive --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
chcp 65001 >nul
where cargo >nul 2>&1 || (echo Не найден cargo. Установите Rust: winget install Rustlang.Rustup & pause & exit /b 1)
pushd "%~dp0source\src-tauri"
cargo build --release || (popd & echo Сборка не удалась & pause & exit /b 1)
popd
taskkill /IM mihomo-tray.exe /F >nul 2>&1
copy /y "%~dp0source\src-tauri\target\release\mihomo-tray.exe" "%~dp0mihomo-tray.exe" >nul
echo Готово: %~dp0mihomo-tray.exe
pause
