@echo off
setlocal
title Antimetro Uninstaller

echo ===================================================
echo   Uninstalling Antimetro from Antigravity 2.0
echo ===================================================
echo.

set "PLUGIN_DIR=%USERPROFILE%\.gemini\config\plugins\antimetro"

if exist "%PLUGIN_DIR%" (
    del /F /Q "%PLUGIN_DIR%\sidecars\antimetro\sidecar.json" >nul 2>&1
    del /F /Q "%PLUGIN_DIR%\plugin.json" >nul 2>&1
    taskkill /F /IM antimetro.exe >nul 2>&1
    rmdir /S /Q "%PLUGIN_DIR%" >nul 2>&1
    if exist "%PLUGIN_DIR%" (
        timeout /t 1 /nobreak >nul 2>&1
        taskkill /F /IM antimetro.exe >nul 2>&1
        rmdir /S /Q "%PLUGIN_DIR%"
    )
    echo [OK] Antimetro plugin files removed.
) else (
    echo [INFO] Antimetro is not currently installed.
)

echo.
echo Note: Your notes and per-chat settings in
echo   %USERPROFILE%\.gemini\antigravity\sidecar_data\antimetro
echo were kept safe. To remove all data completely, delete that folder manually.
echo Fully restart Antigravity to finish.
echo.
pause
