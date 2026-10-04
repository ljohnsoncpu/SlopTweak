# Run a debug build of the launcher from this checkout, using this checkout's
# catalog/catalog.json instead of the one on GitHub. No release needed.
#
#   pwsh dev/run-dev.ps1            real Vast (SPENDS MONEY, same as the installed app)
#   pwsh dev/run-dev.ps1 -Mock      mock provider, no Vast calls
#
# Real mode copies VAST_API_KEY / CIVITAI_TOKEN from the environment (or
# HKCU\Environment) into Credential Manager if they're missing. Keys are
# never printed. The debug build shares settings and Credential Manager with
# the installed app, so close that first.
#
# Instances still fetch their setup files from the instance-v0.1.5 bundle
# (SLOPTWEAK_DEV_ASSETS_URL overrides it); changes under instance/ need a bundle
# you built.

[CmdletBinding()]
param(
    [switch]$Mock,
    [int]$CatalogPort = 18557
)

$ErrorActionPreference = "Stop"
$app = Split-Path -Parent $PSScriptRoot
$catalogDir = Join-Path (Split-Path -Parent $app) "catalog"

if (-not (Test-Path (Join-Path $catalogDir "catalog.json"))) {
    throw "catalog.json not found in $catalogDir"
}
if (Get-Process -Name "sloptweak" -ErrorAction SilentlyContinue) {
    Write-Warning "A sloptweak process is already running. It shares settings with the debug build; close it first."
}

$server = $null
try {
    $server = Start-Process -FilePath "python" `
        -ArgumentList @("-m", "http.server", $CatalogPort, "--bind", "127.0.0.1", "--directory", "`"$catalogDir`"") `
        -WindowStyle Hidden -PassThru
    $env:SLOPTWEAK_CATALOG_URL = "http://127.0.0.1:$CatalogPort/catalog.json"
    if ($Mock) {
        $env:SLOPTWEAK_PROVIDER = "mock"
        Write-Host "Mock provider: no Vast calls."
    } else {
        $env:SLOPTWEAK_DEV_IMPORT_KEYS = "1"
        Write-Host "REAL Vast: this spends money. Stop the session in the app when you're done."
    }
    Write-Host "Catalog: $env:SLOPTWEAK_CATALOG_URL ($catalogDir)"

    Push-Location $app
    try {
        if (-not (Test-Path "node_modules")) { npm install }
        npx tauri dev
    } finally {
        Pop-Location
    }
} finally {
    if ($server -and -not $server.HasExited) { Stop-Process -Id $server.Id -Force }
}
