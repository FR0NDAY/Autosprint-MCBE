@echo off
echo ========================================
echo   Building Better Autosprint (Rust)...
echo ========================================

cargo build --release

if %ERRORLEVEL% NEQ 0 (
    echo.
    echo [!] Build FAILED!
    exit /b %ERRORLEVEL%
)

echo.
echo [OK] Build Successful: target\release\autosprint-mcbe.exe
