# build.ps1 [rom...] [-Keys aes_keys.txt] [-Out folder] [-Precache]
#
# build.sh for Windows: builds Pocket Native and games for the handheld from
# your own ROMs, .3ds/.cci or .cia files, compressed (.zcci/.zcia) or not,
# with your console's aes_keys.txt where they are encrypted, inside a
# container (Docker Desktop or Podman). With no ROM it asks for them.
# -Precache (or PN_PRECACHE=1) lists the shaders the games' models need for
# the handheld to build when a game first starts. Leaves "Pocket Native" in
# the output folder, build\ by default, to copy into roms\ports on the card.
param(
    [Parameter(Position = 0, ValueFromRemainingArguments = $true)][string[]]$Rom = @(),
    [string]$Keys = "",
    [string]$Out = "",
    [switch]$Precache
)
if ($Precache) { $env:PN_PRECACHE = "1" }

$repo = Split-Path -Parent $PSScriptRoot
$Host.UI.RawUI.WindowTitle = "Pocket Native"

function Pick($title, $filter, $many) {
    Add-Type -AssemblyName System.Windows.Forms
    $dialog = New-Object System.Windows.Forms.OpenFileDialog -Property @{ Title = $title; Filter = $filter; Multiselect = $many }
    # an owner on top keeps the dialog from opening behind the console
    $owner = New-Object System.Windows.Forms.Form -Property @{ TopMost = $true }
    $picked = "$($dialog.ShowDialog($owner))" -eq "OK"
    $owner.Dispose()
    # the keyboard goes back to the console, for what the build asks next
    [void](New-Object -ComObject WScript.Shell).AppActivate("Pocket Native")
    if (-not $picked) { return @() }
    return $dialog.FileNames
}

if ($Rom.Count -eq 0) {
    $Rom = @(Pick "Pick the games to build" "3DS games (*.3ds;*.cci;*.cia;*.zcci;*.zcia)|*.3ds;*.cci;*.cia;*.zcci;*.zcia|All files|*.*" $true)
    if ($Rom.Count -eq 0) {
        Write-Host "no game picked"
        exit 2
    }
}
foreach ($file in $Rom) {
    if (-not (Test-Path -LiteralPath $file -PathType Leaf)) {
        Write-Host "usage: scripts\build.ps1 [rom...] [-Keys aes_keys.txt] [-Out folder]"
        Write-Host "no such file: $file"
        exit 2
    }
}
$Rom = @($Rom | ForEach-Object { (Resolve-Path -LiteralPath $_).Path })
if ($Keys) { $Keys = (Resolve-Path -LiteralPath $Keys -ErrorAction Stop).Path }
if (-not $Out) { $Out = Join-Path $repo "build" }
New-Item -ItemType Directory -Force -Path $Out | Out-Null
$Out = (Resolve-Path -LiteralPath $Out).Path

# a game takes the keys given, else the aes_keys.txt beside it or in the
# kit, for an encrypted one, and a .cia else the one picked when asked
function KeysFor($file) {
    if ($script:Keys) { return $script:Keys }
    foreach ($near in @((Join-Path (Split-Path -Parent $file) "aes_keys.txt"), (Join-Path $repo "aes_keys.txt"))) {
        if (Test-Path -LiteralPath $near -PathType Leaf) { return $near }
    }
    if (@(".cia", ".zcia") -notcontains [IO.Path]::GetExtension($file).ToLower()) { return $null }
    $picked = @(Pick "A .cia needs your console's aes_keys.txt, pick it" "aes_keys.txt|aes_keys.txt|Text files (*.txt)|*.txt" $false)
    if ($picked.Count -gt 0) { $script:Keys = $picked[0] }
    return $script:Keys
}

$engine = @("docker", "podman") | Where-Object { Get-Command $_ -ErrorAction SilentlyContinue } | Select-Object -First 1
if (-not $engine) {
    Write-Host "the build needs Docker Desktop or Podman"
    if (-not (Get-Command winget -ErrorAction SilentlyContinue)) { exit 1 }
    $answer = Read-Host "install Docker Desktop with winget now? (y/n)"
    if ($answer -ne "y") { exit 1 }
    winget install -e --id Docker.DockerDesktop
    Write-Host "start Docker Desktop, wait until it is running, then run this script again"
    exit 1
}
& $engine info 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) {
    Write-Host "$engine is not running, start it and run this script again"
    exit 1
}
$hash = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $repo "scripts\container\Containerfile")).Hash.ToLower().Substring(0, 12)
$image = "pocket-native-build:$hash"
& $engine image inspect $image 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) {
    Write-Host "== making the build container, once"
    & $engine build -f (Join-Path $repo "scripts\container\Containerfile") -t $image (Join-Path $repo "scripts\container")
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}

$failed = @()
foreach ($file in $Rom) {
    # the scratch space stays in a volume on the container's own disk,
    # compiling into a Windows folder is many times slower
    $mounts = @("-v", "${repo}:/src:ro", "-v", "pocket-native-work:/work", "-v", "${Out}:/out", "-v", "$(Split-Path -Parent $file):/rom:ro")
    $arguments = @("/rom/$(Split-Path -Leaf $file)")
    $gameKeys = KeysFor $file
    if ($gameKeys) {
        $mounts += @("-v", "$(Split-Path -Parent $gameKeys):/keys:ro")
        $arguments += "/keys/$(Split-Path -Leaf $gameKeys)"
    }
    & $engine run --rm @mounts -e CARGO_HOME=/work/cargo -e CARGO_TARGET_DIR=/work/target -e HOME=/work -e "PN_PRECACHE=$env:PN_PRECACHE" `
        $image bash /src/scripts/container/inside.sh @arguments
    if ($LASTEXITCODE -ne 0) { $failed += $file }
}
& (Join-Path $PSScriptRoot "update.ps1") -Check
if ($failed.Count -gt 0) {
    Write-Host "== these did not build:"
    $failed | ForEach-Object { Write-Host "   $_" }
    exit 1
}
exit 0
