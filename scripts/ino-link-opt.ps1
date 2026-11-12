# Turns the linker's dead-code removal on or off for every session that uses scripts\ino-env.*
# (it writes %CARGO_HOME%\config.toml, which lives in .build and is not part of the upstream tree).
#   .\scripts\ino-link-opt.ps1 -Enable    # /OPT:REF /OPT:ICF for x86_64-pc-windows-msvc
#   .\scripts\ino-link-opt.ps1 -Disable
# Changing it changes rustflags, so cargo rebuilds the whole workspace once. Run it while no other
# session is building, and tell them to rebuild; all sessions then share the same artifacts.
param([switch]$Enable, [switch]$Disable)
$root = Split-Path (Split-Path $MyInvocation.MyCommand.Path -Parent) -Parent
$cfg = "$root\.build\cargo-home\config.toml"
$marker = '# ino-link-opt'
$old = if (Test-Path $cfg) { Get-Content $cfg -Raw } else { '' }
$kept = ($old -split "`n" | Where-Object { $_ -notmatch 'ino-link-opt' -and $_ -notmatch '^\[target\.x86_64-pc-windows-msvc\]$' -and $_ -notmatch '^rustflags = \["-C", "link-arg=/OPT' }) -join "`n"
if ($Enable) {
    $kept = $kept.TrimEnd() + "`n$marker`n[target.x86_64-pc-windows-msvc]`nrustflags = [`"-C`", `"link-arg=/OPT:REF`", `"-C`", `"link-arg=/OPT:ICF`"]`n"
    Set-Content $cfg $kept -Encoding utf8; "enabled: $cfg"
} elseif ($Disable) {
    Set-Content $cfg $kept.TrimEnd() -Encoding utf8; "disabled: $cfg"
} else { if (Test-Path $cfg) { Get-Content $cfg } else { 'no config' } }
