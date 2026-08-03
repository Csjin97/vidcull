@echo off
setlocal
set "SCRIPT_DIR=%~dp0"

powershell -NoProfile -ExecutionPolicy Bypass -File "%SCRIPT_DIR%app\scripts\build-release.ps1"
set "EXIT_CODE=%ERRORLEVEL%"

echo.
if %EXIT_CODE% NEQ 0 (
    echo Build failed with exit code %EXIT_CODE%.
) else (
    echo Build succeeded.
)

pause
endlocal
exit /b %EXIT_CODE%
