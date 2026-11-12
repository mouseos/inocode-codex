# Shared build environment for every InoCode session (PowerShell). Usage:
#   . scripts\ino-env.ps1          # CLI (codex-rs) -> cd codex-rs; cargo ... --profile dev-small -p <crate>
#   . scripts\ino-env.ps1 -Gui     # GUI -> cd gui\inocode-desktop; cargo build -j 1
# Only paths and cargo behaviour live here (no RUSTFLAGS / CARGO_PROFILE_*), so artifact hashes
# are identical across sessions and nothing is rebuilt or duplicated.
param([switch]$Gui)
$root = Split-Path (Split-Path $MyInvocation.MyCommand.Path -Parent) -Parent
New-Item -ItemType Directory -Force "$root\.build\cargo-home", "$root\.build\tmp" | Out-Null
$env:TMP = "$root\.build\tmp"; $env:TEMP = $env:TMP
$env:CARGO_NET_GIT_FETCH_WITH_CLI = 'true'
$env:CARGO_INCREMENTAL = '0'
$env:CARGO_BUILD_JOBS = '8'   # 24 CPUs x several GB per rustc ran out of memory (23.6GB RAM)
$env:RUST_MIN_STACK = '67108864'   # 64MB; 256MB x many rustc threads crashed rustc (0xc0000409)
foreach ($n in 'HTTPS_PROXY', 'HTTP_PROXY', 'ALL_PROXY', 'NODE_EXTRA_CA_CERTS') { Remove-Item "Env:$n" -ErrorAction SilentlyContinue }
if ($Gui) {
    $env:CARGO_TARGET_DIR = [IO.Path]::GetFullPath("$root\..\..\gui\inocode-desktop\target")
    Remove-Item Env:CARGO_HOME -ErrorAction SilentlyContinue
    $env:RUST_MIN_STACK = '268435456'   # GUI builds with -j 1
} else {
    $env:CARGO_HOME = "$root\.build\cargo-home"
    $env:CARGO_TARGET_DIR = "$root\codex-rs\target-win"
}

# cargo under the shared lock (scripts\ino-lock.ps1; a dead owner's lock is removed automatically),
# then remove stale copies/old executables. Use instead of plain cargo.
function ino-cargo {
    & "$root\scripts\ino-lock.ps1" -Action Acquire -OwnerPid $PID
    if ($LASTEXITCODE -ne 0) { return }
    try {
        & cargo @args
        $rc = $LASTEXITCODE
        & "$root\scripts\ino-clean.ps1" | Out-Null
        $global:LASTEXITCODE = $rc
    } finally { & "$root\scripts\ino-lock.ps1" -Action Release -OwnerPid $PID }
}
