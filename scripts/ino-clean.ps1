# Removes stale build output. Safe to run any time: files in use (running exe) cannot be deleted
# on Windows and are skipped. Never touches the newest executable of each name.
#   .\scripts\ino-clean.ps1            # CLI target (codex-rs\target-win)
#   .\scripts\ino-clean.ps1 -Gui       # GUI target (gui\inocode-desktop-codex\target)
#   .\scripts\ino-clean.ps1 -DryRun    # only report
#   .\scripts\ino-clean.ps1 -Incremental   # also remove the (unused, CARGO_INCREMENTAL=0) incremental dir
param([switch]$Gui, [switch]$DryRun, [switch]$Incremental, [double]$MinAgeHours = 1)
$root = Split-Path (Split-Path $MyInvocation.MyCommand.Path -Parent) -Parent
$target = if ($Gui) { [IO.Path]::GetFullPath("$root\..\..\gui\inocode-desktop-codex\target") } else { "$root\codex-rs\target-win" }
$freed = 0L
function Remove-Stale($f) {
    $size = $f.Length
    if ($DryRun) { $script:freed += $size; return }
    try { Remove-Item -LiteralPath $f.FullName -Force -ErrorAction Stop; $script:freed += $size } catch { }
}
foreach ($profileDir in Get-ChildItem $target -Directory -ErrorAction SilentlyContinue) {
    # 1. copies left by swap/run scripts: name.old*, name.new*, name.running-*
    Get-ChildItem $profileDir.FullName -File -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match '\.(old|new|running)([-.].*)?\.(exe|pdb)$' } | ForEach-Object { Remove-Stale $_ }
    # 2. older hashed test/bin executables in deps: keep the newest per name
    $deps = Join-Path $profileDir.FullName 'deps'
    if (Test-Path $deps) {
        # Names shared by many crates (all, build_script_build, ...) are skipped: "newest per name"
        # would delete another crate's valid binary and force a recompile.
        Get-ChildItem $deps -Filter *.exe -File |
            Where-Object { $_.BaseName -notmatch '^(all|main|tests?|suite|integration.*|build[-_]script.*)-[0-9a-f]{16}$' } |
            Group-Object { $_.BaseName -replace '-[0-9a-f]{16}$', '' } | ForEach-Object {
            $_.Group | Sort-Object LastWriteTime -Descending | Select-Object -Skip 1 |
                Where-Object { $_.LastWriteTime -lt (Get-Date).AddHours(-$MinAgeHours) } | ForEach-Object {
                $pdb = [IO.Path]::ChangeExtension($_.FullName, '.pdb')
                Remove-Stale $_
                if (Test-Path $pdb) { Remove-Stale (Get-Item $pdb) }
            }
        }
    }
    if ($Incremental) {
        $inc = Join-Path $profileDir.FullName 'incremental'
        if (Test-Path $inc) {
            $size = (Get-ChildItem $inc -Recurse -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum
            if (-not $DryRun) { Remove-Item $inc -Recurse -Force -ErrorAction SilentlyContinue }
            $freed += $size
        }
    }
}
"{0} {1:N1} GB freed under {2}" -f $(if ($DryRun) { 'would free' } else { 'freed' }), ($freed / 1GB), $target
