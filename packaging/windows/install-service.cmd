@echo off
REM Install rusty_time as a Windows service. Run from an elevated prompt.
REM
REM LocalSystem is used because it already holds SeSystemtimePrivilege, which
REM is the single privilege rtimed needs. It does NOT disable the Windows Time
REM service: two daemons disciplining one clock will fight, so stopping w32time
REM is left as an explicit choice (see the note at the bottom).

setlocal
set "BINDIR=%~dp0"
set "EXE=%BINDIR%rtimed.exe"

if not exist "%EXE%" (
    echo ERROR: rtimed.exe not found next to this script.
    exit /b 1
)

net session >nul 2>&1
if errorlevel 1 (
    echo ERROR: this script must be run from an elevated prompt.
    exit /b 1
)

echo Installing service...
sc.exe create rusty_time binPath= "\"%EXE%\" serve --nts" start= auto DisplayName= "rusty_time NTP/NTS"
if errorlevel 1 goto :failed

sc.exe description rusty_time "Pure-Rust NTP/NTS time service"
sc.exe failure rusty_time reset= 86400 actions= restart/5000/restart/5000/restart/30000

echo Starting service...
sc.exe start rusty_time
if errorlevel 1 goto :failed

echo.
echo Installed. Check it with:  rtimec serverstats
echo.
echo NOTE: the Windows Time service (w32time) may also be disciplining this
echo       clock. Two time daemons on one clock will fight each other. To hand
echo       timekeeping to rusty_time:  sc.exe config w32time start= disabled
echo                                   sc.exe stop w32time
exit /b 0

:failed
echo.
echo Install failed. To remove a partial install:  sc.exe delete rusty_time
exit /b 1
