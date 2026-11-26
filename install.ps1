# qh installer for Windows:  irm https://raw.githubusercontent.com/kesavsabari/qh-rs/main/install.ps1 | iex
#
# What it checks before installing anything:
#  - It reads the release's manifest, qh-release.json (each binary's name, size and SHA-256), and
#    downloads exactly the version it names, checking size and SHA-256. Any mismatch: nothing is
#    installed.
#  - The manifest's Ed25519 signature (the qh release key, the same one src/update.rs pins) is
#    checked only if an OpenSSL 3 is available: on PATH, or the one Git for Windows ships.
#    Windows PowerShell and .NET have no built-in Ed25519, so without it the signature is NOT
#    checked and the installer says so. The hash then only proves the binary matches the manifest
#    next to it on GitHub (catches a corrupted or swapped download, not a forged release).
#    $env:QH_REQUIRE_SIGNATURE = '1' refuses to install in that case. Every later `qh update`
#    checks the signature itself.
# Failures throw (never `exit`, which would close the shell running `irm | iex`).
$ErrorActionPreference = 'Stop'
$repo = 'kesavsabari/qh-rs'
# Testing only: a local mirror laid out like GitHub's releases (latest/download/<f>, download/vX.Y.Z/<f>).
# The release key isn't overridable.
$rel = if ($env:QH_RELEASES_URL) { $env:QH_RELEASES_URL } else { "https://github.com/$repo/releases" }
$asset = 'qh-windows-x86_64.exe'
# the qh release key (Ed25519, SPKI PEM); must equal PUBLIC_KEY in src/update.rs
$releaseKey = "-----BEGIN PUBLIC KEY-----`nMCowBQYDK2VwAyEAeQFv3hp0tMxmR7sjLEnaWjeezjPGhXclr4asTLQfYlg=`n-----END PUBLIC KEY-----`n"
# RFC 8032 test vector 2 (message "r"): tells "this openssl can't verify Ed25519" from "bad signature"
$probeKey = "-----BEGIN PUBLIC KEY-----`nMCowBQYDK2VwAyEAPUAXw+hDiVqStwqnTRt+vJyYLM8uxJaMwM1V8Sr0Zgw=`n-----END PUBLIC KEY-----`n"
$probeSig = 'kqAJqfDUyrhyDoILX2QlQKKye1QWUD+Ps3YiI+vbadoIWsHkPhWZbkWPNhPQ8R2MOHsurrQwKu6wDSkWErsMAA=='

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("qh-install-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
try {
    function Test-Ed25519($exe, $pem, $msg, $sig) {
        $ErrorActionPreference = 'Continue'
        try { & $exe pkeyutl -verify -pubin -inkey $pem -rawin -in $msg -sigfile $sig 2>$null | Out-Null; return ($LASTEXITCODE -eq 0) }
        catch { return $false }
    }
    $enc = New-Object Text.UTF8Encoding($false)
    [IO.File]::WriteAllText((Join-Path $tmp 'probe.pem'), $probeKey, $enc)
    [IO.File]::WriteAllBytes((Join-Path $tmp 'probe.sig'), [Convert]::FromBase64String($probeSig))
    [IO.File]::WriteAllText((Join-Path $tmp 'probe.msg'), 'r', $enc)
    $candidates = @((Get-Command openssl -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty Source))
    foreach ($g in @($env:ProgramFiles, ${env:ProgramFiles(x86)}, (Join-Path $env:LOCALAPPDATA 'Programs'))) {
        if ($g) { $candidates += (Join-Path $g 'Git\usr\bin\openssl.exe'), (Join-Path $g 'Git\mingw64\bin\openssl.exe') }
    }
    $ossl = $null
    foreach ($c in $candidates) {
        if ($c -and (Test-Path $c) -and (Test-Ed25519 $c (Join-Path $tmp 'probe.pem') (Join-Path $tmp 'probe.msg') (Join-Path $tmp 'probe.sig'))) { $ossl = $c; break }
    }

    Write-Host "Downloading qh..."
    $manifest = Join-Path $tmp 'qh-release.json'
    try { Invoke-WebRequest -Uri "$rel/latest/download/qh-release.json" -OutFile $manifest -UseBasicParsing }
    catch { throw "qh installer: couldn't download the release manifest (qh-release.json); the newest release may not be signed yet. Nothing was installed." }
    if ($ossl) {
        $sig = Join-Path $tmp 'qh-release.json.sig'
        try { Invoke-WebRequest -Uri "$rel/latest/download/qh-release.json.sig" -OutFile $sig -UseBasicParsing }
        catch { throw "qh installer: the latest release has no signature (qh-release.json.sig). Nothing was installed." }
        [IO.File]::WriteAllText((Join-Path $tmp 'release.pem'), $releaseKey, $enc)
        if (-not (Test-Ed25519 $ossl (Join-Path $tmp 'release.pem') $manifest $sig)) {
            throw "qh installer: the release manifest's signature doesn't match the qh release key. Nothing was installed."
        }
        Write-Host "Release signature verified."
    } elseif ($env:QH_REQUIRE_SIGNATURE -eq '1') {
        throw "qh installer: QH_REQUIRE_SIGNATURE=1 but no OpenSSL 3 found to verify Ed25519 (install Git for Windows, which ships one). Nothing was installed."
    } else {
        Write-Warning "No OpenSSL 3 found, so the release signature was NOT checked; only the download's SHA-256 against the release manifest is. (Install Git for Windows and rerun to check it.)"
    }

    $m = Get-Content -Raw -Path $manifest | ConvertFrom-Json
    $ver = [string]$m.version
    if ($ver -notmatch '^\d+\.\d+\.\d+$') { throw "qh installer: the release manifest has no version. Nothing was installed." }
    $entry = @($m.assets | Where-Object { $_.name -eq $asset })
    if ($entry.Count -ne 1 -or [string]$entry[0].sha256 -notmatch '^[0-9a-fA-F]{64}$') {
        throw "qh installer: qh $ver's manifest has no (single) entry for $asset. Nothing was installed."
    }
    $want = ([string]$entry[0].sha256).ToLower()
    $bin = Join-Path $tmp $asset
    Invoke-WebRequest -Uri "$rel/download/v$ver/$asset" -OutFile $bin -UseBasicParsing
    $size = (Get-Item $bin).Length
    $got = (Get-FileHash -Algorithm SHA256 -Path $bin).Hash.ToLower()
    if ($size -ne [long]$entry[0].size -or $got -ne $want) {
        throw "qh installer: $asset doesn't match the release manifest (expected $($entry[0].size) bytes, sha256 $want; got $size bytes, sha256 $got). Nothing was installed."
    }
    Write-Host "Checked $asset against the release manifest (qh $ver, sha256 $got)."

    $dir  = Join-Path $env:LOCALAPPDATA 'qh\bin'
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    Move-Item -Force -Path $bin -Destination (Join-Path $dir 'qh.exe')
} finally {
    Remove-Item -Recurse -Force -Path $tmp -ErrorAction SilentlyContinue
}

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
