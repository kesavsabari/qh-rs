#!/bin/sh
# qh installer for Linux/macOS:  curl -fsSL kesavsabari.github.io/qh/sh | sh
#
# What it checks before installing anything:
#  - It reads the release's signed manifest, qh-release.json (each binary's name, size and SHA-256),
#    and checks its Ed25519 signature against the qh release key pinned below (the same key that
#    src/update.rs pins, so the same check `qh update` does). That needs OpenSSL 3 (`openssl
#    pkeyutl -rawin`): Ubuntu 22.04+, Debian 12+, Fedora, Arch, and macOS with Homebrew's openssl@3.
#    macOS's own /usr/bin/openssl is LibreSSL and can't, nor can OpenSSL 1.1 (Ubuntu 20.04).
#    Without it the signature is NOT checked and the installer says so: the hash check below then
#    only proves the binary matches the manifest sitting next to it on GitHub, which catches a
#    corrupted or swapped download but not a forged release. QH_REQUIRE_SIGNATURE=1 refuses to
#    install in that case. Every later `qh update` checks the signature itself, with no openssl.
#  - It downloads the binary of exactly the version the manifest names, and checks its size and
#    SHA-256 against the manifest. Any mismatch, missing manifest or bad signature: nothing is
#    installed and it exits non-zero.
set -eu
repo=kesavsabari/qh-rs
# Testing only: QH_RELEASES_URL points the downloads at a local mirror (file:///...) laid out like
# GitHub's releases (latest/download/<file>, download/vX.Y.Z/<file>). The release key isn't
# overridable, so a mirror can't make an unsigned build pass the signature check.
rel=${QH_RELEASES_URL:-https://github.com/$repo/releases}
# The qh release key (Ed25519 public key, SPKI PEM). Must equal PUBLIC_KEY in src/update.rs.
release_key='-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAeQFv3hp0tMxmR7sjLEnaWjeezjPGhXclr4asTLQfYlg=
-----END PUBLIC KEY-----'
# RFC 8032 test vector 2 (message "r"), used only to find out whether an openssl can verify Ed25519,
# so that "can't verify" is never confused with "signature is bad".
probe_key='-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAPUAXw+hDiVqStwqnTRt+vJyYLM8uxJaMwM1V8Sr0Zgw=
-----END PUBLIC KEY-----'
probe_sig=kqAJqfDUyrhyDoILX2QlQKKye1QWUD+Ps3YiI+vbadoIWsHkPhWZbkWPNhPQ8R2MOHsurrQwKu6wDSkWErsMAA==

# opt-in desktop app (Linux): QH_APP=1 curl ... | sh   or   curl ... | sh -s -- --app
for a in "$@"; do [ "$a" = --app ] && QH_APP=1; done
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)  asset=qh-linux-x86_64 ;;
  Darwin-arm64)  asset=qh-macos-arm64 ;;
  Darwin-x86_64) asset=qh-macos-x86_64 ;;
  *) echo "no prebuilt binary for $(uname -s) $(uname -m); build from source with cargo"; exit 1 ;;
esac

die() { echo "qh installer: $*; nothing was installed" >&2; exit 1; }
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' INT TERM

if command -v sha256sum >/dev/null 2>&1; then sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else die "need sha256sum or shasum to check the download"; fi

# an openssl that really verifies Ed25519 (checked against the known-good vector)
ossl=
for o in openssl /opt/homebrew/opt/openssl@3/bin/openssl /usr/local/opt/openssl@3/bin/openssl; do
  command -v "$o" >/dev/null 2>&1 || continue
  printf '%s\n' "$probe_key" > "$tmp/probe.pem"
  printf '%s\n' "$probe_sig" | "$o" base64 -d -A > "$tmp/probe.sig" 2>/dev/null || continue
  printf r > "$tmp/probe.msg"
  if "$o" pkeyutl -verify -pubin -inkey "$tmp/probe.pem" -rawin -in "$tmp/probe.msg" -sigfile "$tmp/probe.sig" >/dev/null 2>&1; then
    ossl=$o; break
  fi
done

echo "Downloading qh..."
curl -fsSL "$rel/latest/download/qh-release.json" -o "$tmp/qh-release.json" \
  || die "couldn't download the release manifest (qh-release.json); the newest release may not be signed yet, try again later"
if [ -n "$ossl" ]; then
  curl -fsSL "$rel/latest/download/qh-release.json.sig" -o "$tmp/qh-release.json.sig" \
    || die "the latest release has no signature (qh-release.json.sig)"
  printf '%s\n' "$release_key" > "$tmp/release.pem"
  "$ossl" pkeyutl -verify -pubin -inkey "$tmp/release.pem" -rawin -in "$tmp/qh-release.json" -sigfile "$tmp/qh-release.json.sig" >/dev/null 2>&1 \
    || die "the release manifest's signature doesn't match the qh release key"
  echo "Release signature verified."
elif [ "${QH_REQUIRE_SIGNATURE:-0}" = 1 ]; then
  die "QH_REQUIRE_SIGNATURE=1 but no openssl here can verify Ed25519 (install OpenSSL 3, e.g. brew install openssl@3)"
else
  echo "warning: no OpenSSL 3 here, so the release signature was NOT checked; only the download's" >&2
  echo "         SHA-256 against the release manifest is. (Install OpenSSL 3 and rerun to check it.)" >&2
fi

# the manifest is small JSON written by scripts/sign-release.sh; flatten it and read it with sed
flat=$(tr -d ' \t\r\n' < "$tmp/qh-release.json")
version=$(printf '%s\n' "$flat" | sed -n 's/^{.*"version":"\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)".*/\1/p')
[ -n "$version" ] || die "the release manifest has no version"
# entry <name> prints "<size> <sha256>" for that file in the manifest, or fails
entry() {
  e=$(printf '%s\n' "$flat" | tr '{}' '\n' | grep -F "\"name\":\"$1\"" || true)
  [ "$(printf '%s\n' "$e" | grep -c .)" = 1 ] || return 1
  s=$(printf '%s\n' "$e" | sed -n 's/.*"size":\([0-9][0-9]*\).*/\1/p')
  h=$(printf '%s\n' "$e" | sed -n 's/.*"sha256":"\([0-9a-fA-F]\{64\}\)".*/\1/p' | tr 'A-F' 'a-f')
  [ -n "$s" ] && [ -n "$h" ] && echo "$s $h"
}
# fetch <name> <dest>: download that file of $version and check it against the manifest
fetch() {
  want=$(entry "$1") || die "qh $version's manifest has no (single) entry for $1"
  curl -fsSL "$rel/download/v$version/$1" -o "$2" || die "couldn't download $1 for qh $version"
  verify "$1" "$2" "$want"
}
verify() {
  got="$(wc -c < "$2" | tr -d ' ') $(sha256 "$2")"
  [ "$got" = "$3" ] || die "$1 doesn't match the release manifest (expected size+sha256 $3, got $got)"
}

dir="$HOME/.local/bin"
fetch "$asset" "$tmp/$asset"
echo "Checked $asset against the release manifest (qh $version, sha256 $(sha256 "$tmp/$asset"))."
mkdir -p "$dir"
cp "$tmp/$asset" "$dir/qh.tmp"
chmod +x "$dir/qh.tmp"
mv "$dir/qh.tmp" "$dir/qh"
echo "Installed $dir/qh"

# Put ~/.local/bin on PATH for new terminals (macOS doesn't have it by default).
case ":$PATH:" in
  *":$dir:"*) ready=1 ;;
  *)
    ready=0
    # shellcheck disable=SC2016 # single-quoted on purpose: written to the rc file, expanded there
    line='export PATH="$HOME/.local/bin:$PATH"'
    # shellcheck disable=SC2016
    case "$(basename "${SHELL:-sh}")" in
      zsh)  rc="$HOME/.zshrc" ;;
      bash) if [ "$(uname -s)" = Darwin ]; then rc="$HOME/.bash_profile"; else rc="$HOME/.bashrc"; fi ;;
      fish) rc="$HOME/.config/fish/conf.d/qh.fish"; line='fish_add_path -g $HOME/.local/bin'; mkdir -p "$(dirname "$rc")" ;;
      *)    rc="$HOME/.profile" ;;
    esac
    if ! grep -qs '\.local/bin' "$rc"; then
      printf '\n# added by the qh installer\n%s\n' "$line" >> "$rc"
      echo "Added $dir to PATH in $rc"
    fi
    ;;
esac

# the desktop app, only when asked (QH_APP=1 or --app); QH_APP_TARBALL=/path installs a local tarball (testing).
# A downloaded app tarball is installed only if the signed manifest lists it.
if [ "${QH_APP:-0}" = 1 ]; then
  if [ "$(uname -s)-$(uname -m)" != Linux-x86_64 ]; then
    echo "qh-app: no prebuilt app for $(uname -s) $(uname -m); skipped"
  else
    if [ -n "${QH_APP_TARBALL:-}" ]; then cp "$QH_APP_TARBALL" "$tmp/app.tar.gz"
    else echo "Downloading qh-app..."
      fetch qh-app-linux-x86_64.tar.gz "$tmp/app.tar.gz"; fi
    tar -xzf "$tmp/app.tar.gz" -C "$tmp"
    src="$tmp/qh-app-linux-x86_64"
    share="$HOME/.local/share"
    install -Dm755 "$src/bin/qh-app" "$dir/qh-app"
    install -Dm644 "$src/share/applications/dev.qh.App.desktop" "$share/applications/dev.qh.App.desktop"
    sed -i "s|^Exec=qh-app|Exec=$dir/qh-app|" "$share/applications/dev.qh.App.desktop"
    install -Dm644 "$src/share/icons/hicolor/scalable/apps/dev.qh.App.svg" "$share/icons/hicolor/scalable/apps/dev.qh.App.svg"
    for n in 128 256; do
      f="$src/share/icons/hicolor/${n}x${n}/apps/dev.qh.App.png"
      if [ -f "$f" ]; then install -Dm644 "$f" "$share/icons/hicolor/${n}x${n}/apps/dev.qh.App.png"; fi
    done
    install -Dm644 "$src/share/metainfo/dev.qh.App.metainfo.xml" "$share/metainfo/dev.qh.App.metainfo.xml"
    if command -v update-desktop-database >/dev/null 2>&1; then update-desktop-database -q "$share/applications" || true; fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then gtk-update-icon-cache -qtf "$share/icons/hicolor" || true; fi
    echo "Installed $dir/qh-app (needs gtk4 + libadwaita; launch \"qh\" from your app menu)"
  fi
fi

echo
if [ "$ready" = 1 ]; then
  echo "Run: qh"
else
  echo "Open a new terminal and run: qh"
  echo "(or in this one: export PATH=\"\$HOME/.local/bin:\$PATH\" && qh)"
fi
echo "Needs a model: Ollama (https://ollama.com) + 'ollama pull qwen3.5:9b', or 'qh login openrouter'."
