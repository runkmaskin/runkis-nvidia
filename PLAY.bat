@echo off
rem Runkis NVIDIA build (VIctory-NVIDIA + local tuning). No game files are included: you need your own copy.
rem Use: drag game_scarlett_master.exe onto this file, or put the game folder next to it as "game\".
rem Settings measured on a Ryzen 5 5500X3D + RTX 2070 SUPER: Series S, 540p half scale, about 18 fps.
setlocal
cd /d "%~dp0"
set "GAME=%~1"
if "%GAME%"=="" set "GAME=%~dp0game\game_scarlett_master.exe"
if not exist "%GAME%" (
  echo Cannot find game_scarlett_master.exe. Drag it onto PLAY.bat, or put the game folder here as "game".
  pause
  exit /b 1
)
for %%I in ("%GAME%") do set "GAMEDIR=%%~dpI"
rem Indoor haze (developer value 1.5), steady light in crowds (0-1), skip unused ray-tracing skinning passes.
set "VN_AIR=0.3"
set "VN_STEADY=0.7"
set "VN_SKIP_BVH=1"
powershell -NoProfile -Command "Get-Process victory,victory_sm -ErrorAction SilentlyContinue | Stop-Process -Force"
set "VICTORY_CACHE_DIR=%~dp0cache"
set "VICTORY_DATA_DIR=%~dp0data"
set "VICTORY_GAME_DIR=%GAMEDIR:~0,-1%"
echo First start translates shaders and builds pipelines: slow for the first minutes, smooth after.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\play-nvidia.ps1" -Exe "%GAME%" -NoSmoothMotion -Resolution 540
if errorlevel 1 (
  echo.
  echo The game ended with an error. See "%~dp0data" for crashes.log
  pause
)
