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
$code = 0
try { & (Join-Path $dir 'qh.exe') --version | Out-Null; $code = $LASTEXITCODE } catch {}
if ($code -eq -1073741515) {
    Write-Warning "qh can't start: the Microsoft Visual C++ Runtime is missing."
    Write-Host "Run: winget install Microsoft.VCRedist.2015+.x64"
    Write-Host "Or: https://aka.ms/vs/17/release/vc_redist.x64.exe"
} else { Write-Host "Installed. Run: qh" }
Write-Host "Needs a model: install Ollama (https://ollama.com) and run 'ollama pull qwen3.5:9b',"
Write-Host "or use a cloud provider: qh login openrouter"
