# pgbx installer for Windows: the pgbx CLI (+ the agent skill). The pgbx extension itself is Linux-only.
#
#   powershell -c "irm https://pgbx.deemwar.com/install.ps1 | iex"
#   & ([scriptblock]::Create((irm https://pgbx.deemwar.com/install.ps1))) -Version v0.5.0 -NoSkill
#
# Env: PGBX_SKILL=no (or yes) skips the skill prompt for the one-liner.
# Parameters: -Version vX.Y.Z (default latest) -Skill (install skill without asking) -NoSkill
#             -InstallDir DIR (default %LOCALAPPDATA%\pgbx\bin) -BaseUrl URL (mirror/testing)
# Downloads come from https://github.com/deemwar-products/pgbx/releases/latest/download/<asset> (no API call)
# and are verified against that release's SHA256SUMS; a mismatch aborts.
param(
    [string]$Version = "",
    [switch]$Skill,
    [switch]$NoSkill,
    [string]$InstallDir = "",
    [string]$BaseUrl = ""
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$Repo = 'deemwar-products/pgbx'

# throw, not exit: under `irm | iex` exit would close the user's PowerShell window
function Fail($msg) { throw "pgbx install: $msg" }

$cpu = $env:PROCESSOR_ARCHITECTURE
if (-not $cpu) { $cpu = "$([Runtime.InteropServices.RuntimeInformation]::OSArchitecture)".ToUpper() -replace '^X64$', 'AMD64' }
$arch = switch ($cpu) {
    'AMD64' { 'amd64' }
    'ARM64' { 'amd64' }   # Windows on ARM runs the x64 build through its built-in emulation
    default { Fail "unsupported CPU $cpu (amd64 and arm64 only)" }
}
if (-not $BaseUrl) {
    if ($Version) {
        if (-not $Version.StartsWith('v')) { $Version = "v$Version" }
        $BaseUrl = "https://github.com/$Repo/releases/download/$Version"
    } else {
        $BaseUrl = "https://github.com/$Repo/releases/latest/download"
    }
}
$BaseUrl = $BaseUrl.TrimEnd('/')
if (-not $InstallDir) { $InstallDir = Join-Path $env:LOCALAPPDATA 'pgbx\bin' }

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("pgbx-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force $tmp | Out-Null
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    $asset = "pgbx-windows-$arch.zip"
    Write-Host "pgbx $(if ($Version) { $Version } else { 'latest' }) (windows/$arch) from $BaseUrl"
    Invoke-WebRequest -UseBasicParsing "$BaseUrl/SHA256SUMS" -OutFile "$tmp\SHA256SUMS"
    $want = (Get-Content "$tmp\SHA256SUMS" | ForEach-Object {
        $p = $_ -split '\s+', 2
        if ($p.Count -eq 2 -and $p[1].TrimStart('*') -eq $asset) { $p[0] }
    } | Select-Object -First 1)
    if (-not $want) { Fail "$asset is not listed in SHA256SUMS" }
    Invoke-WebRequest -UseBasicParsing "$BaseUrl/$asset" -OutFile "$tmp\$asset"
    $got = (Get-FileHash -Algorithm SHA256 "$tmp\$asset").Hash.ToLower()
    if ($got -ne $want.ToLower()) { Fail "checksum mismatch for $asset (expected $want, got $got); nothing was installed" }

    Expand-Archive -Force "$tmp\$asset" "$tmp\x"
    $exe = Get-ChildItem -Recurse "$tmp\x" -Filter pgbx.exe | Select-Object -First 1
    if (-not $exe) { Fail "pgbx.exe missing from $asset" }
    New-Item -ItemType Directory -Force $InstallDir | Out-Null
    Copy-Item -Force $exe.FullName (Join-Path $InstallDir 'pgbx.exe')
    $pgbx = Join-Path $InstallDir 'pgbx.exe'
    Write-Host "installed $pgbx ($(& $pgbx --version))"

    # example connection adapters (ssh, aws, gcp, azure; Node scripts) where `pgbx profile add --adapter ssh` finds them
    $ad = Get-ChildItem -Recurse -Directory "$tmp\x" -Filter adapters | Select-Object -First 1
    if ($ad) {
        $adDir = if ($env:PGBX_ADAPTERS_DIR) { $env:PGBX_ADAPTERS_DIR } else { Join-Path $env:APPDATA 'pgbx\adapters' }
        New-Item -ItemType Directory -Force $adDir | Out-Null
        Copy-Item -Recurse -Force (Join-Path $ad.FullName '*') $adDir
        Write-Host "example adapters: $adDir (need Node 18+ only if you use one)"
    }

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not (($userPath -split ';') -contains $InstallDir)) {
        [Environment]::SetEnvironmentVariable('Path', ((@($userPath, $InstallDir) | Where-Object { $_ }) -join ';'), 'User')
        Write-Host "added $InstallDir to your user PATH (open a new terminal to use pgbx)"
    }
    $env:Path = "$InstallDir;$env:Path"

    $doSkill = $true
    # PGBX_SKILL=no|yes lets the `irm | iex` one-liner (which cannot take parameters) skip the prompt
    if ($NoSkill -or $env:PGBX_SKILL -eq 'no') { $doSkill = $false }
    elseif ($Skill -or $env:PGBX_SKILL -eq 'yes') { $doSkill = $true }
    # never prompt without a real console (CI, -NonInteractive, piped): Read-Host would hang
    elseif ([Environment]::UserInteractive -and -not [Console]::IsInputRedirected -and
            -not ([Environment]::GetCommandLineArgs() -match '^-NonI') -and -not $env:CI) {
        $ans = Read-Host "Install the pgbx agent skill for Claude Code / Codex (~/.claude/skills/pgbx-skill)? [Y/n]"
        if ($ans -match '^[Nn]') { $doSkill = $false }
    }
    if ($doSkill) {
        & $pgbx skill install
        if ($LASTEXITCODE -eq 0) { Write-Host "skill installed (remove with: pgbx skill uninstall)" }
        else { Write-Host "skill install failed; retry later with: pgbx skill install" -ForegroundColor Yellow }
    }
    Write-Host ""
    Write-Host "The pgbx extension runs on Linux Postgres servers (PostgreSQL 13-18); install it there with:"
    Write-Host "  curl -fsSL https://pgbx.deemwar.com/install.sh | sh"
    Write-Host "From Windows, pgbx talks to a server over TCP: pgbx status --host db.example.com --user postgres"
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
