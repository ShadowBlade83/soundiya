@echo off
wsl --cd "%~dp0." bash -lc "bash scripts/build-arm.sh && bash scripts/package.sh"
if errorlevel 1 (
    echo.
    echo Build failed.
) else (
    echo.
    echo Done: dist\soundiya-armv7
)
pause
