@echo off
rem Export the assembly in front in Inventor and build it in SolidCraft.
rem Put inv2sc.exe next to this file (or on PATH). The export goes to %TEMP%\inv2sc-export
rem (or the folder given as INV2SC_EXPORT) and the SolidCraft designs to its SolidCraft folder.
setlocal
set HERE=%~dp0
set EXPORT=%TEMP%\inv2sc-export
if not "%INV2SC_EXPORT%"=="" set EXPORT=%INV2SC_EXPORT%
powershell.exe -NoProfile -STA -ExecutionPolicy Bypass -File "%HERE%Export-InventorBundle.ps1" -OutFolder "%EXPORT%" -Mode Active
if errorlevel 1 goto failed
set INV2SC=%HERE%inv2sc.exe
if not exist "%INV2SC%" set INV2SC=inv2sc.exe
"%INV2SC%" convert "%EXPORT%" --verbose %*
if errorlevel 1 goto failed
echo.
echo Done. The designs and conversion-report.md are in %EXPORT%\SolidCraft
pause
exit /b 0
:failed
echo.
echo The conversion stopped. See the messages above.
pause
exit /b 1
