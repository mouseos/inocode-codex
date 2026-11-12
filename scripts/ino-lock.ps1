# The one cargo lock shared by every session: directory .build\cargo-lock with owner.txt inside
# (pid, process start time, command). A lock whose owner process no longer exists is removed
# automatically, so a crashed or killed build can never block everyone.
#   .\scripts\ino-lock.ps1 -Action Acquire [-OwnerPid N] [-TimeoutSec 21600]
#   .\scripts\ino-lock.ps1 -Action Release [-OwnerPid N]     # only the owner releases (-Force: anyone)
#   .\scripts\ino-lock.ps1 -Action Status
# OwnerPid is the long-lived process that holds the lock (default: this script's parent, i.e. your shell).
param(
    [ValidateSet('Acquire', 'Release', 'Status')][string]$Action = 'Status',
    [int]$OwnerPid = 0,
    [int]$TimeoutSec = 21600,
    [switch]$Force
)
$root = Split-Path (Split-Path $MyInvocation.MyCommand.Path -Parent) -Parent
$lock = "$root\.build\cargo-lock"
$ownerFile = "$lock\owner.txt"
if ($OwnerPid -le 0) { $OwnerPid = (Get-CimInstance Win32_Process -Filter "ProcessId=$PID").ParentProcessId }

function Get-Owner {
    if (-not (Test-Path $ownerFile)) { return $null }
    $o = @{}
    foreach ($l in Get-Content $ownerFile) { if ($l -match '^(\w+)=(.*)$') { $o[$Matches[1]] = $Matches[2] } }
    $o
}
function Test-OwnerAlive($o) {
    if (-not $o -or -not $o.pid) { return $false }
    $p = Get-Process -Id ([int]$o.pid) -ErrorAction SilentlyContinue
    if (-not $p) { return $false }
    # pid reuse guard: the process must have started when the owner recorded
    try { return $p.StartTime.ToUniversalTime().ToString('o') -eq $o.procstart } catch { return $true }
}
function Test-Stale {
    if (-not (Test-Path $lock)) { return $false }
    $o = Get-Owner
    if ($o) { return -not (Test-OwnerAlive $o) }
    # lock without owner.txt: made by an old script, or its owner died between mkdir and writing the file
    return ((Get-Date) - (Get-Item $lock).CreationTime).TotalMinutes -gt 15
}
function Write-Owner {
    $p = Get-Process -Id $OwnerPid -ErrorAction SilentlyContinue
    $start = if ($p) { $p.StartTime.ToUniversalTime().ToString('o') } else { '' }
    $cmd = (Get-CimInstance Win32_Process -Filter "ProcessId=$OwnerPid" -ErrorAction SilentlyContinue).CommandLine
    "pid=$OwnerPid`nprocstart=$start`nlocked=$((Get-Date).ToUniversalTime().ToString('o'))`ncmd=$cmd" | Set-Content $ownerFile -Encoding utf8
}

switch ($Action) {
    'Status' {
        if (-not (Test-Path $lock)) { 'free'; break }
        $o = Get-Owner
        if ($o) { "held by pid $($o.pid) (alive: $(Test-OwnerAlive $o)) since $($o.locked): $($o.cmd)" } else { 'held, no owner.txt' }
    }
    'Release' {
        $o = Get-Owner
        if ((Test-Path $lock) -and ($Force -or -not $o -or [int]$o.pid -eq $OwnerPid)) { Remove-Item $lock -Recurse -Force -ErrorAction SilentlyContinue }
    }
    'Acquire' {
        $deadline = (Get-Date).AddSeconds($TimeoutSec)
        $told = $false
        while ($true) {
            if (Test-Stale) { Remove-Item $lock -Recurse -Force -ErrorAction SilentlyContinue; "removed stale cargo-lock" | Write-Host }
            try {
                New-Item -ItemType Directory $lock -ErrorAction Stop | Out-Null
                Write-Owner
                exit 0
            } catch {
                if (-not $told) { "waiting for cargo-lock: $(& $PSCommandPath -Action Status)" | Write-Host; $told = $true }
                if ((Get-Date) -gt $deadline) { 'cargo-lock timeout' | Write-Host; exit 2 }
                Start-Sleep -Seconds 5
            }
        }
    }
}
