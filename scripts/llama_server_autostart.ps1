# Logon autostart for IGRIS's llama-server, mirroring llm.rs::ensure_server:
# GPU build first, CPU fallback. One extra guard IGRIS does not have:
# after /health comes up, a 1-token generation probe verifies the server
# can actually GENERATE - the GPU crash seen 2026-11-17 (0xc0000005 in
# nvoglv64.dll mid-generation) leaves /health answering but the process
# dying on the first real request, which IGRIS's health+props check
# cannot catch. Flags are try_start's, verbatim, so a boot-spawned
# server is configuration-identical to an app-spawned one and the
# "reusing already-running llama-server" reuse path accepts it.
# Never kills a foreign server on the port (fail-closed like
# ensure_server); IGRIS itself handles a mismatched port loudly.
$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
$gpuExe = Join-Path $root 'assets\llama-cpp\vulkan\llama-server.exe'
$cpuExe = Join-Path $root 'assets\llama-cpp\cpu\llama-server.exe'
$model = Join-Path $root 'assets\llama-cpp\models\LFM3-Omni-2B-Q4_K_M.gguf'
$port = 8081
$stderrLog = Join-Path $env:TEMP 'igris-llama-server-stderr.log'
$bootLog = Join-Path $env:TEMP 'igris-llama-boot.log'
function Log($m) {
    Add-Content -Path $bootLog -Value ("[{0}] {1}" -f (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'), $m) -Encoding UTF8
}
function Server-Health {
    try { Invoke-WebRequest "http://127.0.0.1:$port/health" -UseBasicParsing -TimeoutSec 2 | Out-Null; $true }
    catch { $false }
}
function Server-IsOurs {
    try {
        $p = Invoke-WebRequest "http://127.0.0.1:$port/props" -UseBasicParsing -TimeoutSec 3
        return $p.Content -like '*LFM3*'
    } catch { return $false }
}
function Test-Generation {
    # 1 token is enough to prove the driver survives a forward pass.
    try {
        $body = '{"prompt":"hi","max_tokens":1}'
        Invoke-WebRequest "http://127.0.0.1:$port/v1/completions" -Method Post `
            -Body $body -ContentType 'application/json' -UseBasicParsing -TimeoutSec 60 | Out-Null
        return $true
    } catch { return $false }
}
function Start-Backend($exe, $gpu) {
    if (-not (Test-Path $exe)) { Log "missing exe: $exe"; return $null }
    $args = @('-m', $model, '-c', '8000', '-fa', 'on', '--swa-full',
        '--cache-reuse', '1', '--no-webui', '--port', "$port")
    if ($gpu) { $args += @('-ngl', '99') }
    Remove-Item $stderrLog -ErrorAction SilentlyContinue
    $p = Start-Process -FilePath $exe -ArgumentList $args -WindowStyle Hidden `
        -RedirectStandardError $stderrLog -PassThru
    Log ("spawned {0} pid {1}" -f (Split-Path $exe -Leaf), $p.Id)
    $deadline = (Get-Date).AddSeconds(90)
    while ((Get-Date) -lt $deadline) {
        if ($p.HasExited) {
            Log ("{0} exited before health (code {1}); last stderr: {2}" -f
                (Split-Path $exe -Leaf), $p.ExitCode,
                ((Get-Content $stderrLog -Tail 1 -ErrorAction SilentlyContinue)))
            return $null
        }
        if (Server-Health) {
            if (Test-Generation) { return $p.Id }
            Log ("{0} answered /health but generation failed" -f (Split-Path $exe -Leaf))
            break
        }
        Start-Sleep -Seconds 1
    }
    if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue }
    return $null
}

if (Server-Health) {
    if (Server-IsOurs) {
        Log "llama-server already running with our model - nothing to do"
        exit 0
    }
    Log "port $port answers /health but is not our llama-server; leaving it alone (IGRIS will refuse loudly)"
    exit 1
}
if (-not (Test-Path $model)) { Log "model missing: $model"; exit 1 }

Log "autostart: trying the GPU build first"
$pid2 = Start-Backend $gpuExe $true
if (-not $pid2) {
    Log "GPU build unusable - falling back to the CPU build"
    $pid2 = Start-Backend $cpuExe $false
}
if ($pid2) { Log "llama-server ready (pid $pid2)"; exit 0 }
Log "no backend could be started; IGRIS will retry at its next run"
exit 1
