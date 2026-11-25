$ErrorActionPreference = "Stop"

$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
$root = $PSScriptRoot
$pluginDir  = Join-Path $HOME ".gemini\config\plugins\antimetro"
$sidecarDir = Join-Path $pluginDir "sidecars\antimetro"
$assetsDir  = Join-Path $pluginDir "assets"

if (-not (Get-Command link.exe -ErrorAction SilentlyContinue)) {
    $vcvars = $null
    $vsw = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path $vsw) {
        $vsRoot = & $vsw -latest -products * -property installationPath | Select-Object -First 1
        if ($vsRoot) {
            $candidate = Join-Path $vsRoot "VC\Auxiliary\Build\vcvars64.bat"
            if (Test-Path $candidate) { $vcvars = $candidate }
        }
    }
    if (-not $vcvars) {
        $fallback = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
        if (Test-Path $fallback) { $vcvars = $fallback }
    }
    if ($vcvars) {
        Write-Host "Loading Visual Studio build environment..." -ForegroundColor DarkCyan
        cmd.exe /c "call `"$vcvars`" >nul && set" | ForEach-Object {
            if ($_ -match '^([^=]+)=(.*)$') {
                [System.Environment]::SetEnvironmentVariable($matches[1], $matches[2], "Process")
            }
        }
    }
}

Write-Host "1/3 Building Antimetro v1.3.0 (Rust release)..." -ForegroundColor Cyan
Push-Location $root
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "Rust build failed. Run this script from 'x64 Native Tools Command Prompt for VS'." }
} finally {
    Pop-Location
}

Write-Host "2/3 Installing plugin into $pluginDir..." -ForegroundColor Cyan
New-Item -ItemType Directory -Path $sidecarDir, $assetsDir -Force | Out-Null
Stop-Process -Name "antimetro" -Force -ErrorAction SilentlyContinue

$exeSrc = Join-Path $root "target\release\antimetro.exe"
$exeDst = Join-Path $sidecarDir "antimetro.exe"
try {
    Copy-Item -Path $exeSrc -Destination $exeDst -Force
} catch {
    Start-Sleep -Milliseconds 250
    Stop-Process -Name "antimetro" -Force -ErrorAction SilentlyContinue
    Copy-Item -Path $exeSrc -Destination $exeDst -Force
}
Copy-Item -Path (Join-Path $root "assets\logo.svg") -Destination (Join-Path $assetsDir "logo.svg") -Force

$pluginJson = "{`n" +
  "  `"name`": `"antimetro`",`n" +
  "  `"displayName`": `"Antimetro`",`n" +
  "  `"version`": `"1.3.0`",`n" +
  "  `"description`": `"Live context token meter, smart auto-compact, per-chat notes, and full transcript search for Antigravity App 2.0.`",`n" +
  "  `"logo`": `"assets/logo.svg`",`n" +
  "  `"author`": {`n" +
  "    `"name`": `"izomondo`",`n" +
  "    `"url`": `"https://izomondo.com`"`n" +
  "  },`n" +
  "  `"homepage`": `"https://izomondo.com`",`n" +
  "  `"repository`": `"https://github.com/izomondo/antimetro`"`n" +
  "}"
[System.IO.File]::WriteAllText((Join-Path $pluginDir "plugin.json"), $pluginJson, $utf8NoBom)

$exeJson = $exeDst.Replace('\', '\\').Replace('"', '\"')
$sidecarJson = "{`n" +
  "  `"command`": `"$exeJson`",`n" +
  "  `"args`": [],`n" +
  "  `"env`": {},`n" +
  "  `"restart_policy`": `"always`",`n" +
  "  `"display_name`": `"Antimetro`",`n" +
  "  `"description`": `"Live context token meter, smart auto-compact, per-chat notes, and full transcript search.`",`n" +
  "  `"has_web_ui`": true,`n" +
  "  `"agent_permissions`": {`n" +
  "    `"allows_user_interactions`": false`n" +
  "  },`n" +
  "  `"ui_config`": {`n" +
  "    `"display_name`": `"Antimetro`",`n" +
  "    `"views`": [`n" +
  "      {`n" +
  "        `"path`": `"/`",`n" +
  "        `"entrypoint`": `"SIDECAR_UI_ENTRYPOINT_AUX_PANE`",`n" +
  "        `"title`": `"Antimetro`"`n" +
  "      }`n" +
  "    ]`n" +
  "  }`n" +
  "}"
[System.IO.File]::WriteAllText((Join-Path $sidecarDir "sidecar.json"), $sidecarJson, $utf8NoBom)

$kb = [math]::Round((Get-Item $exeDst).Length / 1KB, 1)
Write-Host "3/3 DONE! Antimetro v1.3.0 installed ($kb KB)." -ForegroundColor Green
Write-Host "Fully restart Antigravity (including the tray icon) to load the update." -ForegroundColor Yellow
