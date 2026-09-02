# Agent Steroids installer for Windows.
#
#   irm https://raw.githubusercontent.com/KenKaiii/agent-steroids/main/install.ps1 | iex
#
# Downloads the release binary, checks it against the SHA256SUMS published
# with the release, and puts it on your user PATH. No Rust toolchain needed.
# From then on `steroids upgrade` keeps it current.
#
# Environment:
#   STEROIDS_VERSION  pin a release, e.g. v0.3.1 (default: latest)
#   INSTALL_DIR       where to put the binary (default: %LOCALAPPDATA%\steroids\bin,
#                     or %USERPROFILE%\.cargo\bin when that already holds one)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Repo = 'KenKaiii/agent-steroids'
$Bin = 'steroids.exe'

function Step($label, $value) { Write-Host ("  {0,-12} {1}" -f $label, $value) }
function Fail($message, $hint) {
    Write-Host ""
    Write-Host "steroids install: $message" -ForegroundColor Red
    if ($hint) { Write-Host $hint }
    exit 1
}

Write-Host "Agent Steroids installer"

# Only x86_64 is released for Windows; ARM runs it through emulation.
$Target = 'x86_64-pc-windows-msvc'
Step 'platform' $Target

# --- version -----------------------------------------------------------------

$Version = $env:STEROIDS_VERSION
if ($Version) {
    if ($Version -notmatch '^v') { $Version = "v$Version" }
} else {
    try {
        $Version = (Invoke-RestMethod -UseBasicParsing "https://api.github.com/repos/$Repo/releases/latest").tag_name
    } catch {
        Fail 'could not work out the latest release' "GitHub may be rate limiting you. Pin one: `$env:STEROIDS_VERSION='v0.3.1'"
    }
}
# A tag is vX.Y.Z and nothing else; anything odd from the API stops here
# rather than becoming part of a URL.
if ($Version -notmatch '^v\d+\.\d+\.\d+$') { Fail "unexpected release tag '$Version'" }
Step 'release' $Version

# --- install dir -------------------------------------------------------------

$CargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if ($env:INSTALL_DIR) {
    $Dir = $env:INSTALL_DIR
} elseif (Test-Path (Join-Path $CargoBin $Bin)) {
    # A cargo install is already on PATH; replace it rather than shadow it.
    $Dir = $CargoBin
} else {
    $Dir = Join-Path $env:LOCALAPPDATA 'steroids\bin'
}
New-Item -ItemType Directory -Force -Path $Dir | Out-Null
Step 'install to' $Dir

# --- download and verify -----------------------------------------------------

$Asset = "steroids-$Target.tar.gz"
$Base = "https://github.com/$Repo/releases/download/$Version"
$Tmp = Join-Path ([IO.Path]::GetTempPath()) ("steroids-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $Tmp | Out-Null

try {
    # Every release publishes SHA256SUMS; a missing or mismatched entry is a
    # reason to stop, not to skip, exactly as `steroids upgrade` treats it.
    try {
        Invoke-WebRequest -UseBasicParsing "$Base/SHA256SUMS" -OutFile (Join-Path $Tmp 'SHA256SUMS')
    } catch {
        Fail "could not download SHA256SUMS for $Version" "Check the release: https://github.com/$Repo/releases/tag/$Version"
    }
    $Expected = $null
    foreach ($line in Get-Content (Join-Path $Tmp 'SHA256SUMS')) {
        $parts = $line.Trim() -split '\s+'
        if ($parts.Length -ge 2 -and $parts[1].TrimStart('*') -eq $Asset) { $Expected = $parts[0].ToLowerInvariant() }
    }
    if (-not $Expected) { Fail "SHA256SUMS has no entry for $Asset" }

    $Installed = Join-Path $Dir $Bin
    $UpToDate = $false
    if (Test-Path $Installed) {
        $current = (& $Installed --version 2>$null)
        $UpToDate = ($current -eq "steroids $($Version.TrimStart('v'))")
    }
    if ($UpToDate) {
        Step 'already at' "$Version, nothing to do"
    } else {
        Step 'downloading' $Asset
        $Archive = Join-Path $Tmp $Asset
        try {
            Invoke-WebRequest -UseBasicParsing "$Base/$Asset" -OutFile $Archive
        } catch {
            Fail "could not download $Base/$Asset"
        }
        $Actual = (Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($Actual -ne $Expected) {
            Fail "checksum mismatch for $Asset" "expected $Expected, got $Actual. The download was corrupted or tampered with; nothing was installed."
        }
        Step 'checksum' 'ok'

        # Extract only the one file the archive is supposed to hold, into the
        # temp dir, so an unexpected archive layout cannot write anywhere else.
        # tar.exe ships with Windows 10 1803+.
        & tar.exe -xzf $Archive -C $Tmp $Bin 2>$null
        $Extracted = Join-Path $Tmp $Bin
        if (-not (Test-Path $Extracted)) { Fail "the archive does not contain $Bin" }

        # Sibling name then rename: a running steroids is never overwritten in place.
        $Staged = "$Installed.new"
        Copy-Item -LiteralPath $Extracted -Destination $Staged -Force
        Move-Item -LiteralPath $Staged -Destination $Installed -Force
        Step 'installed' $Installed
    }
} finally {
    Remove-Item -Recurse -Force -LiteralPath $Tmp -ErrorAction SilentlyContinue
}

# --- PATH --------------------------------------------------------------------

$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($UserPath -split ';') -notcontains $Dir) {
    [Environment]::SetEnvironmentVariable('Path', "$Dir;$UserPath", 'User')
    $env:Path = "$Dir;$env:Path"
    Write-Host ""
    Write-Host "  Added $Dir to your user PATH. Open a new terminal for it to take effect."
}

Write-Host ""
Write-Host "  Next: steroids add BurntSushi/ripgrep    (or hand the README to your agent)"
