param([string]$OutputDirectory)
$ErrorActionPreference = 'Stop'
# 将已构建的 EXE 与实际运行依赖组成完整目录；不打包个人配置和缓存。
$repo = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) { $OutputDirectory = Join-Path $repo 'dist\holocubic-display' }
if (Test-Path -LiteralPath $OutputDirectory) { throw 'Output already exists; choose a new empty output directory.' }
$binary = Join-Path $repo 'target\release\rs_holocubic.exe'
if (-not (Test-Path -LiteralPath $binary)) { throw 'Run cargo build --release --locked from the repository root first.' }
$runtime = Join-Path $OutputDirectory 'rs_holocubic'
New-Item -ItemType Directory -Path $runtime -Force | Out-Null
Copy-Item -LiteralPath $binary -Destination (Join-Path $runtime 'rs_holocubic.exe')
foreach ($file in Get-ChildItem -LiteralPath (Join-Path $repo 'rs_holocubic') -Filter '*.py' -File) {
    if (-not $file.Name.StartsWith('test_')) { Copy-Item -LiteralPath $file.FullName -Destination $runtime }
}
foreach ($name in @('holo_usb_display.py','requirements.txt','README.md','LICENSE','THIRD_PARTY_NOTICES.md','CONTRIBUTING.md')) {
    Copy-Item -LiteralPath (Join-Path $repo $name) -Destination $OutputDirectory
}
Copy-Item -LiteralPath (Join-Path $repo 'rs_holocubic\README.md') -Destination $runtime
Copy-Item -LiteralPath (Join-Path $repo 'rs_holocubic\assets') -Destination $runtime -Recurse
Copy-Item -LiteralPath (Join-Path $repo 'device_app') -Destination $OutputDirectory -Recurse
Copy-Item -LiteralPath (Join-Path $repo 'docs') -Destination $OutputDirectory -Recurse
Write-Output "Portable package: $OutputDirectory"
