# Upgrade the bundled llama.cpp Vulkan + CPU runtimes in assets/llama-cpp.
#
# Why: b12110 could not use Flash Attention for qwen35 prompt prefill on
# Vulkan (~36 tok/s); b12359 (llama.cpp v1.0.0) added the Vulkan prefill FA
# path and brings Qwen3.5-4B tool rounds from ~7-16 s down to ~1.5-3 s on
# the same RTX 4050. The app's flags are unchanged and compatible.
#
# Usage (from anywhere):
#   powershell -ExecutionPolicy Bypass -File .\scripts\update_llama_binaries.ps1
#
# The old binary folders are backed up to %TEMP%\igris-llama-backup-<stamp>\.
# Stop any running llama-server first: Windows locks the DLLs otherwise.
$ErrorActionPreference = 'Stop'
$tag = 'b12359'
$root = Split-Path -Parent $PSScriptRoot
$targetRoot = Join-Path $root 'assets\llama-cpp'
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$backupRoot = Join-Path $env:TEMP "igris-llama-backup-$stamp"
$work = Join-Path $env:TEMP "igris-llama-update-$stamp"

if (Get-Process -Name llama-server -ErrorAction SilentlyContinue) {
    Write-Error "llama-server is running. Stop it first (Stop-Process -Name llama-server), then rerun."
}
if (-not (Test-Path (Join-Path $targetRoot 'vulkan\llama-server.exe'))) {
    Write-Error "expected assets tree not found at $targetRoot"
}

New-Item -ItemType Directory -Force -Path $work | Out-Null
$builds = @(
    @{ backend = 'vulkan'; url = "https://github.com/ggml-org/llama.cpp/releases/download/$tag/llama-$tag-bin-win-vulkan-x64.zip" },
    @{ backend = 'cpu';     url = "https://github.com/ggml-org/llama.cpp/releases/download/$tag/llama-$tag-bin-win-cpu-x64.zip" }
)

# Download + extract everything BEFORE touching the installed folders.
foreach ($b in $builds) {
    $zip = Join-Path $work "$($b.backend).zip"
    $out = Join-Path $work $b.backend
    Write-Host "downloading $($b.backend) build $tag ..."
    Invoke-WebRequest -Uri $b.url -OutFile $zip -UseBasicParsing -TimeoutSec 600
    New-Item -ItemType Directory -Force -Path $out | Out-Null
    Expand-Archive -Path $zip -DestinationPath $out -Force
    if (-not (Test-Path (Join-Path $out 'llama-server.exe'))) {
        Write-Error "download for $($b.backend) is missing llama-server.exe"
    }
}

foreach ($b in $builds) {
    $target = Join-Path $targetRoot $b.backend
    $source = Join-Path $work $b.backend
    # Refuse a partial swap: the release zip must carry the same file set.
    $oldNames = @(Get-ChildItem $target -File | Select-Object -ExpandProperty Name | Sort-Object)
    $newNames = @(Get-ChildItem $source -File | Select-Object -ExpandProperty Name | Sort-Object)
    if ($oldNames.Count -eq 0 -or (Compare-Object $oldNames $newNames)) {
        Write-Error "file set changed for $($b.backend); refusing to swap. Inspect $source manually."
    }
    $backup = Join-Path $backupRoot $b.backend
    New-Item -ItemType Directory -Force -Path $backup | Out-Null
    Copy-Item "$target\*" -Destination $backup -Force
    Copy-Item "$source\*" -Destination $target -Force
    Write-Host "$($b.backend): installed $tag (old build backed up to $backup)"
}

Write-Host "done. Old builds: $backupRoot"
& (Join-Path $targetRoot 'vulkan\llama-server.exe') --version 2>&1 | Select-Object -First 1
