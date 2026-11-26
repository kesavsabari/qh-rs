#!/bin/sh
# Sign a release on this laptop, after CI has built it:  scripts/sign-release.sh v0.1.4
#
# Writes qh-release.json (the version, and each binary's name, size and SHA-256) and signs it with
# the release key (Ed25519, ~/.config/qh-release/key.pem, never on GitHub). qh only offers and
# installs releases whose signature checks against the public key built into it, so until this
# runs, a new release stays a draft (release.yml makes drafts): not `latest`, so neither the install
# line nor `qh update` sees it. This script uploads the signature and then publishes the release.
#
# The signature vouches for whatever CI built, so before signing this checks where the binaries came
# from (docs/SECURITY-RELEASE.md has the why):
#  1. the tag on GitHub points at the same commit as the tag in this local clone;
#  2. the release workflow run for that commit and tag succeeded;
#  3. every binary has a GitHub build-provenance attestation (Sigstore) saying it was built by
#     .github/workflows/release.yml from that tag and commit, on a GitHub-hosted runner.
# Releases built before release.yml made attestations (v0.2.8 and older) have none:
# QH_SKIP_PROVENANCE=1 skips step 3 for those, loudly. A byte-for-byte local rebuild isn't part of
# the check: the builds aren't reproducible yet (toolchain not pinned, build paths embedded).
#
# The key may be an encrypted PKCS#8 PEM ("ENCRYPTED PRIVATE KEY", see docs/SECURITY-RELEASE.md):
# the passphrase is asked once on the terminal and handed to openssl on stdin, never in argv or env.
# QH_DRY_RUN=1 does every check and the signing, then stops before uploading anything.
set -eu
tag=${1:?usage: scripts/sign-release.sh vX.Y.Z}
repo=kesavsabari/qh-rs
key=${QH_RELEASE_KEY:-$HOME/.config/qh-release/key.pem}
ver=${tag#v}
assets="qh-linux-x86_64 qh-macos-arm64 qh-macos-x86_64 qh-windows-x86_64.exe"
# the apps, signed too when the release carries them (install.sh --app / install-app.ps1 need them listed)
optional="qh-app-linux-x86_64.tar.gz qh-app-windows-x64.zip"

[ -r "$key" ] || { echo "no release key at $key"; exit 1; }
here=$(cd "$(dirname "$0")/.." && pwd)
d=$(mktemp -d)
trap 'rm -r "$d"' EXIT

# openssl with the release key: asks for the passphrase once if the key is encrypted
pass=
if grep -q 'BEGIN ENCRYPTED PRIVATE KEY' "$key"; then
  [ -r /dev/tty ] || { echo "the release key is encrypted and there's no terminal to ask for its passphrase"; exit 1; }
  printf 'passphrase for %s: ' "$key" > /dev/tty
  stty -echo < /dev/tty
  IFS= read -r pass < /dev/tty || true
  stty echo < /dev/tty
  printf '\n' > /dev/tty
fi
withkey() {  # withkey <openssl args...>: openssl with -inkey/-in "$key" given by the caller
  if [ -n "$pass" ]; then printf '%s\n' "$pass" | openssl "$@" -passin stdin
  else openssl "$@"; fi
}
withkey pkey -in "$key" -pubout -out "$d/pub.pem" || { echo "couldn't read the release key (wrong passphrase?)"; exit 1; }

# the key must be the one qh trusts (the public key in src/update.rs)
want=$(grep -A1 'const PUBLIC_KEY' "$here/src/update.rs" | grep -o '[0-9]\+' | tail -n 32 | tr '\n' ' ')
have=$(openssl pkey -pubin -in "$d/pub.pem" -outform DER | tail -c 32 | od -An -tu1 | tr -s ' \n' ' ' | sed 's/^ //')
[ "$want" = "$have" ] || { echo "this key isn't the one in src/update.rs"; exit 1; }
# the tag's Cargo.toml must carry the same version, or the new qh won't report it
git -C "$here" show "$tag:Cargo.toml" | grep -q "^version = \"$ver\"" || { echo "$tag's Cargo.toml isn't version $ver"; exit 1; }

# 1. GitHub's tag is this clone's tag
commit=$(git -C "$here" rev-parse "$tag^{commit}")
remote=$(git ls-remote "https://github.com/$repo" "refs/tags/$tag" "refs/tags/$tag^{}" | tail -n 1 | cut -f1)
[ "$remote" = "$commit" ] || { echo "$tag is $commit here but ${remote:-missing} on GitHub; not signing"; exit 1; }
# 2. the release workflow ran for that commit and tag, and passed
run=$(gh run list -R "$repo" --workflow release.yml --commit "$commit" --event push \
  --json databaseId,conclusion,headBranch --jq ".[] | select(.headBranch == \"$tag\" and .conclusion == \"success\") | .databaseId" | head -n 1)
[ -n "$run" ] || { echo "no successful release.yml run for $tag at $commit; not signing"; exit 1; }
echo "$tag = $commit, built by release.yml run $run"

echo "downloading $tag's binaries..."
for a in $assets; do
  gh release download "$tag" -R "$repo" -p "$a" -D "$d" || { echo "$tag has no $a (has CI finished?)"; exit 1; }
done
have_assets=$(gh release view "$tag" -R "$repo" --json assets --jq '.assets[].name')
for a in $optional; do
  if printf '%s\n' "$have_assets" | grep -qx "$a"; then
    gh release download "$tag" -R "$repo" -p "$a" -D "$d"
    assets="$assets $a"
  fi
done
# anything on the release that isn't ours (or the manifest) is a red flag
for a in $have_assets; do
  case " $assets $optional qh-release.json qh-release.json.sig " in
    *" $a "*) ;;
    *) echo "$tag carries an unexpected asset: $a; not signing"; exit 1 ;;
  esac
done

# 3. each binary's build provenance
if [ "${QH_SKIP_PROVENANCE:-0}" = 1 ]; then
  echo "WARNING: QH_SKIP_PROVENANCE=1: not checking build attestations. Only for releases built before release.yml made them."
else
  for a in $assets; do
    gh attestation verify "$d/$a" -R "$repo" \
      --signer-workflow "$repo/.github/workflows/release.yml" \
      --source-ref "refs/tags/$tag" --source-digest "$commit" \
      --deny-self-hosted-runners >/dev/null \
      || { echo "$a has no valid build attestation from release.yml at $tag ($commit); not signing"; exit 1; }
    echo "attested: $a"
  done
fi

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

# shellcheck disable=SC2086 # $assets is a list of names
python3 - "$d" "$ver" $assets > "$d/qh-release.json" <<'EOF'
import hashlib, json, os, sys
d, ver, names = sys.argv[1], sys.argv[2], sys.argv[3:]
assets = []
for n in names:
    b = open(os.path.join(d, n), "rb").read()
    assets.append({"name": n, "size": len(b), "sha256": hashlib.sha256(b).hexdigest()})
print(json.dumps({"version": ver, "assets": assets}, indent=1))
EOF
withkey pkeyutl -sign -inkey "$key" -rawin -in "$d/qh-release.json" -out "$d/qh-release.json.sig"
openssl pkeyutl -verify -pubin -inkey "$d/pub.pem" -rawin -in "$d/qh-release.json" -sigfile "$d/qh-release.json.sig" >/dev/null
cat "$d/qh-release.json"
if [ "${QH_DRY_RUN:-0}" = 1 ]; then echo "QH_DRY_RUN=1: checked and signed locally, nothing uploaded"; exit 0; fi
gh release upload "$tag" -R "$repo" --clobber "$d/qh-release.json" "$d/qh-release.json.sig"
# release.yml leaves a new release as a draft; publishing it makes it `latest` for the installers
if [ "$(gh release view "$tag" -R "$repo" --json isDraft --jq .isDraft)" = true ]; then
  gh release edit "$tag" -R "$repo" --draft=false --latest
  echo "published the $tag release"
fi
echo "signed $tag: qh update now offers it"
