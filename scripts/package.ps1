param([string]$OutputDirectory)
$ErrorActionPreference = 'Stop'
# Package the release EXE and device/docs assets; no Python runtime.
$repo = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) { $OutputDirectory = Join-Path $repo 'dist\holocubic-display' }
if (Test-Path -LiteralPath $OutputDirectory) { throw 'Output already exists; choose a new empty output directory.' }
$binary = Join-Path $repo 'target\release\rs_holocubic.exe'
if (-not (Test-Path -LiteralPath $binary)) { throw 'Run cargo build --release --locked from the repository root first.' }
$runtime = Join-Path $OutputDirectory 'rs_holocubic'
New-Item -ItemType Directory -Path $runtime -Force | Out-Null
Copy-Item -LiteralPath $binary -Destination (Join-Path $runtime 'rs_holocubic.exe')
foreach ($name in @('README.md','LICENSE','THIRD_PARTY_NOTICES.md','CONTRIBUTING.md')) {
    Copy-Item -LiteralPath (Join-Path $repo $name) -Destination $OutputDirectory
}
Copy-Item -LiteralPath (Join-Path $repo 'rs_holocubic\README.md') -Destination $runtime
Copy-Item -LiteralPath (Join-Path $repo 'rs_holocubic\assets') -Destination $runtime -Recurse
Copy-Item -LiteralPath (Join-Path $repo 'device_app') -Destination $OutputDirectory -Recurse
Copy-Item -LiteralPath (Join-Path $repo 'docs') -Destination $OutputDirectory -Recurse
Write-Output "Portable package: $OutputDirectory"
