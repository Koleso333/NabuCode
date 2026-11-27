# One-file everyend: release server (nabu.exe) embedded into build\release\everyend-tauri.exe
$ErrorActionPreference = 'Stop'

function Invoke-Step {
    param([string]$Program, [string[]]$Arguments)
    & $Program @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Program failed with exit code $LASTEXITCODE." }
}

$everyend = Split-Path -Parent $PSScriptRoot
$root = Split-Path -Parent $everyend

Push-Location -LiteralPath $root
try {
    $metadata = Invoke-Step cargo @('metadata', '--no-deps', '--format-version', '1') | ConvertFrom-Json
    # The side panel of the Chrome extension goes into extension\panel first: nabu.exe carries the extension
    Invoke-Step npm.cmd @('run', 'build:panel', '--prefix', $everyend)
    Invoke-Step cargo @('build', '--release', '-p', 'nabu-server')
    $env:NABUCODE_SERVER_EXE = Join-Path $metadata.target_directory 'release\nabu.exe'

    Set-Location -LiteralPath $everyend
    Invoke-Step npm.cmd @('run', 'tauri', '--', 'build', '--no-bundle', '--features', 'embed-server')
} finally {
    Pop-Location
}
