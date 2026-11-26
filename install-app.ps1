# qh for Windows, the app:  irm https://raw.githubusercontent.com/kesavsabari/qh-rs/main/install-app.ps1 | iex
# Installs qh itself first if it's missing, then the app into %LOCALAPPDATA%\qh\app with a Start menu
# entry. No admin, nothing outside your user folder. Remove: delete that folder and the shortcut.
# The zip is installed only if the release manifest (qh-release.json) lists it with the same size and
# SHA-256; the manifest's signature is checked when an OpenSSL 3 is available (see install.ps1 for
# what that does and doesn't prove). Failures throw, never `exit` (that would close the shell).
$ErrorActionPreference = 'Stop'
$repo = 'kesavsabari/qh-rs'
$rel = if ($env:QH_RELEASES_URL) { $env:QH_RELEASES_URL } else { "https://github.com/$repo/releases" }
$name = 'qh-app-windows-x64.zip'
$root = Join-Path $env:LOCALAPPDATA 'qh'
if (-not (Test-Path (Join-Path $root 'bin\qh.exe'))) {
    Invoke-RestMethod "https://raw.githubusercontent.com/$repo/main/install.ps1" | Invoke-Expression
}
$releaseKey = "-----BEGIN PUBLIC KEY-----`nMCowBQYDK2VwAyEAeQFv3hp0tMxmR7sjLEnaWjeezjPGhXclr4asTLQfYlg=`n-----END PUBLIC KEY-----`n"
$probeKey = "-----BEGIN PUBLIC KEY-----`nMCowBQYDK2VwAyEAPUAXw+hDiVqStwqnTRt+vJyYLM8uxJaMwM1V8Sr0Zgw=`n-----END PUBLIC KEY-----`n"
$probeSig = 'kqAJqfDUyrhyDoILX2QlQKKye1QWUD+Ps3YiI+vbadoIWsHkPhWZbkWPNhPQ8R2MOHsurrQwKu6wDSkWErsMAA=='
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("qh-app-install-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
$zip = Join-Path $tmp $name
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
    $manifest = Join-Path $tmp 'qh-release.json'
    Invoke-WebRequest -Uri "$rel/latest/download/qh-release.json" -OutFile $manifest -UseBasicParsing
    if ($ossl) {
        $sig = Join-Path $tmp 'qh-release.json.sig'
        Invoke-WebRequest -Uri "$rel/latest/download/qh-release.json.sig" -OutFile $sig -UseBasicParsing
        [IO.File]::WriteAllText((Join-Path $tmp 'release.pem'), $releaseKey, $enc)
        if (-not (Test-Ed25519 $ossl (Join-Path $tmp 'release.pem') $manifest $sig)) {
            throw "qh-app installer: the release manifest's signature doesn't match the qh release key. Nothing was installed."
        }
    } elseif ($env:QH_REQUIRE_SIGNATURE -eq '1') {
        throw "qh-app installer: QH_REQUIRE_SIGNATURE=1 but no OpenSSL 3 found to verify Ed25519. Nothing was installed."
    } else {
        Write-Warning "No OpenSSL 3 found, so the release signature was NOT checked; only the download's SHA-256 against the release manifest is."
    }
    $m = Get-Content -Raw -Path $manifest | ConvertFrom-Json
    $ver = [string]$m.version
    if ($ver -notmatch '^\d+\.\d+\.\d+$') { throw "qh-app installer: the release manifest has no version. Nothing was installed." }
    $entry = @($m.assets | Where-Object { $_.name -eq $name })
    if ($entry.Count -ne 1 -or [string]$entry[0].sha256 -notmatch '^[0-9a-fA-F]{64}$') {
        throw "qh-app installer: release $ver's manifest doesn't list $name, so it can't be checked. Nothing was installed."
    }
    $url = "$rel/download/v$ver/$name"
    Write-Host "Downloading $url"
    Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
    $got = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash.ToLower()
    if ((Get-Item $zip).Length -ne [long]$entry[0].size -or $got -ne ([string]$entry[0].sha256).ToLower()) {
        throw "qh-app installer: $name doesn't match the release manifest. Nothing was installed."
    }
    $app = Join-Path $root 'app'
    # a running app holds its files: close it first
    Get-Process qh-app -ErrorAction SilentlyContinue | Stop-Process -Force
    if (Test-Path $app) { Remove-Item -Recurse -Force $app }
    Expand-Archive -Path $zip -DestinationPath $app
} finally {
    Remove-Item -Recurse -Force -Path $tmp -ErrorAction SilentlyContinue
}
$lnk = Join-Path ([Environment]::GetFolderPath('Programs')) 'qh.lnk'
$s = (New-Object -ComObject WScript.Shell).CreateShortcut($lnk)
$s.TargetPath = Join-Path $app 'qh-app.exe'
$s.WorkingDirectory = $app
$s.Description = 'qh'
$s.Save()
Write-Host "Installed. Open qh from the Start menu."
