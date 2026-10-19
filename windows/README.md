# qh for Windows

WinUI 3 client of `qh serve`, the same app as `apple/` (PLAN.md v0.3.0 item 3).
Unpackaged and self-contained: no Windows App SDK runtime to install.

Build (Windows, .NET 8 SDK):

    dotnet build windows/qh.csproj -c Release -p:Platform=x64

Run `windows/bin/x64/Release/net8.0-windows10.0.19041.0/win-x64/qh-app.exe`. It uses the `qh.exe`
in `%LOCALAPPDATA%\qh\bin` (install.ps1 puts it there) or on PATH. Needs Windows 10 1803+ (AF_UNIX).
