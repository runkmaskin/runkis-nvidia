@echo off
rem Runkis Launcher: builds it the first time (needs the .NET 8 SDK), then starts it.
rem It runs the same tuned settings as PLAY.bat (scripts\play-nvidia.ps1 -PrintEnv), with your profile on top.
setlocal
cd /d "%~dp0"
set "EXE=%~dp0launcher\dist\RunkisLauncher.exe"
if not exist "%EXE%" (
  echo Building the launcher once...
  dotnet publish launcher\src\Runkis.Launcher\Runkis.Launcher.csproj -c Release -r win-x64 --self-contained false -p:PublishSingleFile=true -o launcher\dist
  if errorlevel 1 (
    echo.
    echo The launcher did not build. It needs the .NET 8 SDK: https://dotnet.microsoft.com/download/dotnet/8.0
    pause
    exit /b 1
  )
)
start "" "%EXE%"
