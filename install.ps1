# qh installer for Windows:  irm https://raw.githubusercontent.com/kesavsabari/qh-rs/main/install.ps1 | iex
$ErrorActionPreference = 'Stop'
$repo = 'kesavsabari/qh-rs'
$dir  = Join-Path $env:LOCALAPPDATA 'qh\bin'
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$url = "https://github.com/$repo/releases/latest/download/qh-windows-x86_64.exe"
Write-Host "Downloading $url"
Invoke-WebRequest -Uri $url -OutFile (Join-Path $dir 'qh.exe') -UseBasicParsing
$path = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not ($path -split ';' | Where-Object { $_ -eq $dir })) {
    [Environment]::SetEnvironmentVariable('Path', "$path;$dir", 'User')
    $env:Path += ";$dir"
    Write-Host "Added $dir to your PATH (new terminals pick it up)."
}
Write-Host "Installed. Run: qh"
Write-Host "Needs a model: install Ollama (https://ollama.com) and run 'ollama pull qwen3.5:9b',"
Write-Host "or use a cloud provider: qh login openrouter"
