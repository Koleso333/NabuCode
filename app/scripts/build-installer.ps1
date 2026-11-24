# Installer for everyend: NabuCode.exe with the release server (nabu.exe) next to it, as an NSIS setup
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
    Invoke-Step cargo @('build', '--release', '-p', 'nabu-server')
    $server = Join-Path $metadata.target_directory 'release\nabu.exe'

    # The server's path depends on the machine, so it is added to the config only for this build
    $config = Join-Path $metadata.target_directory 'installer.conf.json'
    @{ bundle = @{ resources = @{ $server = 'nabu.exe' } } } | ConvertTo-Json -Depth 5 | Out-File -LiteralPath $config -Encoding ascii

    Set-Location -LiteralPath $everyend
    # Apart from build\release, where the one-file build with the embedded server lives
    $env:CARGO_TARGET_DIR = Join-Path $everyend 'build\installer'
    Invoke-Step npm.cmd @('run', 'tauri', '--', 'build', '--bundles', 'nsis', '--config', $config)
} finally {
    Pop-Location
}
