@echo off
rem pgbx installer for Windows, plain cmd.exe: no PowerShell, so execution policies never block it.
rem Uses only tools built into Windows 10 1803+ / 11: curl.exe, certutil, tar, reg.
rem   curl -fsSL https://pgbx.deemwar.com/install.cmd -o install.cmd && install.cmd
rem Options: -Version vX.Y.Z (default latest)  -NoSkill  -Skill  -InstallDir DIR  -BaseUrl URL (mirror/testing)
rem Env: PGBX_SKILL=no|yes, PGBX_ADAPTERS_DIR (default %APPDATA%\pgbx\adapters)
rem Downloads are verified against the release's SHA256SUMS; any mismatch aborts before anything is installed.
setlocal EnableExtensions EnableDelayedExpansion
set "REPO=deemwar-products/pgbx"
set "VERSION="
set "SKILL=%PGBX_SKILL%"
set "INSTALLDIR="
set "BASEURL="

:args
if "%~1"=="" goto args_done
if /i "%~1"=="-Version"    (set "VERSION=%~2" & shift & shift & goto args)
if /i "%~1"=="-NoSkill"    (set "SKILL=no" & shift & goto args)
if /i "%~1"=="-Skill"      (set "SKILL=yes" & shift & goto args)
if /i "%~1"=="-InstallDir" (set "INSTALLDIR=%~2" & shift & shift & goto args)
if /i "%~1"=="-BaseUrl"    (set "BASEURL=%~2" & shift & shift & goto args)
echo pgbx install: unknown option %~1 1>&2
exit /b 2
:args_done

for %%T in (curl.exe certutil.exe tar.exe reg.exe) do (
  where %%T >nul 2>&1 || (echo pgbx install: %%T not found ^(needs Windows 10 1803 or newer^) 1>&2 & exit /b 1)
)

rem Windows on ARM runs the x64 build through its built-in emulation: one asset for every Windows
set "ASSET=pgbx-windows-amd64.zip"
if not defined BASEURL (
  if defined VERSION (
    if /i not "!VERSION:~0,1!"=="v" set "VERSION=v!VERSION!"
    set "BASEURL=https://github.com/%REPO%/releases/download/!VERSION!"
  ) else (
    set "BASEURL=https://github.com/%REPO%/releases/latest/download"
  )
)
if "%BASEURL:~-1%"=="/" set "BASEURL=%BASEURL:~0,-1%"
if not defined INSTALLDIR set "INSTALLDIR=%LOCALAPPDATA%\pgbx\bin"
if defined VERSION (set "SHOWN=%VERSION%") else (set "SHOWN=latest")
echo pgbx %SHOWN% (windows/x64) from %BASEURL%

set "TMPD=%TEMP%\pgbx-%RANDOM%%RANDOM%"
mkdir "%TMPD%" || exit /b 1

curl -fsSL --retry 3 -o "%TMPD%\SHA256SUMS" "%BASEURL%/SHA256SUMS" || (echo pgbx install: cannot download %BASEURL%/SHA256SUMS 1>&2 & goto fail)
set "WANT="
for /f "tokens=1,2" %%A in ('type "%TMPD%\SHA256SUMS"') do (
  set "N=%%B"
  if "!N:~0,1!"=="*" set "N=!N:~1!"
  if /i "!N!"=="%ASSET%" set "WANT=%%A"
)
if not defined WANT (echo pgbx install: %ASSET% is not listed in SHA256SUMS 1>&2 & goto fail)

curl -fsSL --retry 3 -o "%TMPD%\%ASSET%" "%BASEURL%/%ASSET%" || (echo pgbx install: cannot download %BASEURL%/%ASSET% 1>&2 & goto fail)
set "GOT="
for /f "skip=1 delims=" %%H in ('certutil -hashfile "%TMPD%\%ASSET%" SHA256') do if not defined GOT set "GOT=%%H"
set "GOT=!GOT: =!"
if /i not "!GOT!"=="!WANT!" (echo pgbx install: checksum mismatch for %ASSET% ^(expected !WANT!, got !GOT!^); nothing was installed 1>&2 & goto fail)

mkdir "%TMPD%\x" >nul 2>&1
tar -xf "%TMPD%\%ASSET%" -C "%TMPD%\x" || (echo pgbx install: cannot unpack %ASSET% 1>&2 & goto fail)
set "EXE="
for /r "%TMPD%\x" %%F in (pgbx.exe) do if exist "%%F" if not defined EXE set "EXE=%%F"
if not defined EXE (echo pgbx install: pgbx.exe missing from %ASSET% 1>&2 & goto fail)
if not exist "%INSTALLDIR%" mkdir "%INSTALLDIR%" || goto fail
copy /y "!EXE!" "%INSTALLDIR%\pgbx.exe" >nul || (echo pgbx install: cannot write %INSTALLDIR%\pgbx.exe ^(is pgbx running?^) 1>&2 & goto fail)
set "PGBX=%INSTALLDIR%\pgbx.exe"
for /f "delims=" %%V in ('"%PGBX%" --version') do echo installed %PGBX% ^(%%V^)

rem example connection adapters (ssh, aws, gcp, azure; Node scripts) where `pgbx profile add --adapter ssh` finds them
set "ADSRC="
for /d /r "%TMPD%\x" %%D in (adapters) do if exist "%%D" if not defined ADSRC set "ADSRC=%%D"
if defined ADSRC (
  if defined PGBX_ADAPTERS_DIR (set "ADDIR=%PGBX_ADAPTERS_DIR%") else (set "ADDIR=%APPDATA%\pgbx\adapters")
  xcopy /e /i /y /q "!ADSRC!" "!ADDIR!" >nul && echo example adapters: !ADDIR! ^(need Node 18+ only if you use one^)
)

rem user PATH (HKCU only; never the machine PATH, never setx, which truncates at 1024 chars)
set "UPATH="
for /f "skip=2 tokens=2,*" %%A in ('reg query "HKCU\Environment" /v Path 2^>nul') do set "UPATH=%%B"
echo ;!UPATH!; | find /i ";%INSTALLDIR%;" >nul
if errorlevel 1 (
  if defined UPATH (set "NEWPATH=!UPATH!;%INSTALLDIR%") else (set "NEWPATH=%INSTALLDIR%")
  reg add "HKCU\Environment" /v Path /t REG_EXPAND_SZ /d "!NEWPATH!" /f >nul && echo added %INSTALLDIR% to your user PATH ^(open a new terminal to use pgbx^)
)

if /i "%SKILL%"=="no" goto skill_done
if /i not "%SKILL%"=="yes" (
  set "ANS=y"
  set /p "ANS=Install the pgbx agent skill for Claude Code / Codex (~/.claude/skills/pgbx-skill)? [Y/n] " <con 2>nul
  if /i "!ANS:~0,1!"=="n" goto skill_done
)
"%PGBX%" skill install && (echo skill installed ^(remove with: pgbx skill uninstall^)) || (echo skill install failed; retry later with: pgbx skill install)
:skill_done

echo.
echo The pgbx extension runs on Linux Postgres servers (PostgreSQL 13-18); install it there with:
echo   curl -fsSL https://pgbx.deemwar.com/install.sh ^| sh
echo From Windows, connect to a server: pgbx setup client prod --url postgres://user@db.example.com/postgres
rmdir /s /q "%TMPD%" >nul 2>&1
exit /b 0

:fail
rmdir /s /q "%TMPD%" >nul 2>&1
exit /b 1
