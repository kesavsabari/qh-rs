#!/bin/sh
# Sign a release on this laptop, after CI has built it:  scripts/sign-release.sh v0.1.4
#
# Writes qh-release.json (the version, and each binary's name, size and SHA-256) and signs it with
# the release key (Ed25519, ~/.config/qh-release/key.pem, never on GitHub). qh only offers and
# installs releases whose signature checks against the public key built into it, so until this
# runs, a new release is downloadable by the install line but not offered by `qh update`.
set -eu
tag=${1:?usage: scripts/sign-release.sh vX.Y.Z}
repo=kesavsabari/qh-rs
key=${QH_RELEASE_KEY:-$HOME/.config/qh-release/key.pem}
ver=${tag#v}
assets="qh-linux-x86_64 qh-macos-arm64 qh-macos-x86_64 qh-windows-x86_64.exe"

[ -r "$key" ] || { echo "no release key at $key"; exit 1; }
here=$(cd "$(dirname "$0")/.." && pwd)
# the key must be the one qh trusts (the public key in src/update.rs)
want=$(grep -A1 'const PUBLIC_KEY' "$here/src/update.rs" | grep -o '[0-9]\+' | tail -n 32 | tr '\n' ' ')
have=$(openssl pkey -in "$key" -pubout -outform DER | tail -c 32 | od -An -tu1 | tr -s ' \n' ' ' | sed 's/^ //')
[ "$want" = "$have" ] || { echo "this key isn't the one in src/update.rs"; exit 1; }
# the tag's Cargo.toml must carry the same version, or the new qh won't report it
git -C "$here" show "$tag:Cargo.toml" | grep -q "^version = \"$ver\"" || { echo "$tag's Cargo.toml isn't version $ver"; exit 1; }

d=$(mktemp -d)
trap 'rm -r "$d"' EXIT
echo "downloading $tag's binaries..."
for a in $assets; do
  gh release download "$tag" -R "$repo" -p "$a" -D "$d" || { echo "$tag has no $a (has CI finished?)"; exit 1; }
done
# this machine's build must run and say it's this version
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) own=qh-linux-x86_64 ;;
  Darwin-arm64) own=qh-macos-arm64 ;;
  *) own= ;;
esac
if [ -n "$own" ]; then
  chmod +x "$d/$own"
  out=$("$d/$own" --version)
  [ "$out" = "qh $ver" ] || { echo "$own reports '$out', not qh $ver"; exit 1; }
fi

python3 - "$d" "$ver" $assets > "$d/qh-release.json" <<'EOF'
import hashlib, json, os, sys
d, ver, names = sys.argv[1], sys.argv[2], sys.argv[3:]
assets = []
for n in names:
    b = open(os.path.join(d, n), "rb").read()
    assets.append({"name": n, "size": len(b), "sha256": hashlib.sha256(b).hexdigest()})
print(json.dumps({"version": ver, "assets": assets}, indent=1))
EOF
openssl pkeyutl -sign -inkey "$key" -rawin -in "$d/qh-release.json" -out "$d/qh-release.json.sig"
openssl pkeyutl -verify -inkey "$key" -rawin -in "$d/qh-release.json" -sigfile "$d/qh-release.json.sig" >/dev/null
cat "$d/qh-release.json"
gh release upload "$tag" -R "$repo" --clobber "$d/qh-release.json" "$d/qh-release.json.sig"
echo "signed $tag: qh update now offers it"
