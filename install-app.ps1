# qh for Windows, the app:  irm https://raw.githubusercontent.com/kesavsabari/qh-rs/main/install-app.ps1 | iex
# Installs qh itself first if it's missing, then the app into %LOCALAPPDATA%\qh\app with a Start menu
# entry. No admin, nothing outside your user folder. Remove: delete that folder and the shortcut.
$ErrorActionPreference = 'Stop'
$repo = 'kesavsabari/qh-rs'
$root = Join-Path $env:LOCALAPPDATA 'qh'
if (-not (Test-Path (Join-Path $root 'bin\qh.exe'))) {
    Invoke-RestMethod "https://raw.githubusercontent.com/$repo/main/install.ps1" | Invoke-Expression
}
$app = Join-Path $root 'app'
$zip = Join-Path $env:TEMP 'qh-app-windows-x64.zip'
$url = "https://github.com/$repo/releases/latest/download/qh-app-windows-x64.zip"
Write-Host "Downloading $url"
Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
# a running app holds its files: close it first
Get-Process qh-app -ErrorAction SilentlyContinue | Stop-Process -Force
if (Test-Path $app) { Remove-Item -Recurse -Force $app }
Expand-Archive -Path $zip -DestinationPath $app
Remove-Item $zip
$lnk = Join-Path ([Environment]::GetFolderPath('Programs')) 'qh.lnk'
$s = (New-Object -ComObject WScript.Shell).CreateShortcut($lnk)
$s.TargetPath = Join-Path $app 'qh-app.exe'
$s.WorkingDirectory = $app
$s.Description = 'qh'
$s.Save()
Write-Host "Installed. Open qh from the Start menu."
