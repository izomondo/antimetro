@echo off
setlocal
title Antimetro v1.4.1 Installer

echo ===================================================
echo   Installing Antimetro v1.4.1 for Antigravity 2.0
echo ===================================================
echo.

set "SRC_DIR=%~dp0"
set "PLUGIN_DIR=%USERPROFILE%\.gemini\config\plugins\antimetro"
set "SIDECAR_DIR=%PLUGIN_DIR%\sidecars\antimetro"
set "ASSETS_DIR=%PLUGIN_DIR%\assets"
set "EXE_DST=%SIDECAR_DIR%\antimetro.exe"

if not exist "%SRC_DIR%antimetro.exe" (
    echo [ERROR] antimetro.exe not found next to Install.bat.
    echo Please unpack the entire ZIP archive before running Install.bat.
    echo.
    pause
    exit /b 1
)

mkdir "%SIDECAR_DIR%" >nul 2>&1
mkdir "%ASSETS_DIR%" >nul 2>&1

taskkill /F /IM antimetro.exe >nul 2>&1
copy /Y "%SRC_DIR%antimetro.exe" "%EXE_DST%" >nul 2>&1
if errorlevel 1 (
    timeout /t 1 /nobreak >nul 2>&1
    taskkill /F /IM antimetro.exe >nul 2>&1
    copy /Y "%SRC_DIR%antimetro.exe" "%EXE_DST%" >nul
    if errorlevel 1 (
        echo [ERROR] Could not copy antimetro.exe. Close Antigravity and try again.
        echo.
        pause
        exit /b 1
    )
)

copy /Y "%SRC_DIR%logo.svg" "%ASSETS_DIR%\logo.svg" >nul

powershell -NoProfile -Command ^
  "$u = New-Object System.Text.UTF8Encoding($false);" ^
  "$pDir = $env:PLUGIN_DIR; $sDir = $env:SIDECAR_DIR;" ^
  "$exe = $env:EXE_DST.Replace('\','\\').Replace('\"','\\\"');" ^
  "$pJson = '{\"name\":\"antimetro\",\"displayName\":\"Antimetro\",\"version\":\"1.4.1\",\"description\":\"Live context token meter, model quota rings, smart auto-compact, per-chat notes, and instant cross-chat search for Antigravity App 2.0.\",\"logo\":\"assets/logo.svg\",\"author\":{\"name\":\"izomondo\",\"url\":\"https://izomondo.com\"},\"homepage\":\"https://izomondo.com\",\"repository\":\"https://github.com/izomondo/antimetro\"}';" ^
  "$sJson = '{\"command\":\"' + $exe + '\",\"args\":[],\"env\":{},\"restart_policy\":\"always\",\"display_name\":\"Antimetro\",\"description\":\"Live context token meter, model quota rings, smart auto-compact, per-chat notes, and instant cross-chat search.\",\"has_web_ui\":true,\"agent_permissions\":{\"allows_user_interactions\":false},\"ui_config\":{\"display_name\":\"Antimetro\",\"views\":[{\"path\":\"/\",\"entrypoint\":\"SIDECAR_UI_ENTRYPOINT_AUX_PANE\",\"title\":\"Antimetro\"}]}}';" ^
  "[System.IO.File]::WriteAllText((Join-Path $pDir 'plugin.json'), $pJson, $u);" ^
  "[System.IO.File]::WriteAllText((Join-Path $sDir 'sidecar.json'), $sJson, $u);"

echo [OK] Antimetro v1.4.1 has been installed!
echo.
echo First-time install: fully restart Antigravity (including the tray icon)
echo and open Antimetro in the right Side Panel.
echo Updating an existing install: just reopen the Antimetro tab.
echo.
pause
