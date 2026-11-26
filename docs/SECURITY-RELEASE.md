# Release security

How a qh release is made and checked, and the GitHub settings the owner has to change by hand
(nothing in this repo can or should change them).

## The release flow

1. Push a tag `vX.Y.Z` (its `Cargo.toml` must say `X.Y.Z`).
2. `release.yml` builds and tests on a **read-only** token, then a job with no checkout makes
   build-provenance attestations (Sigstore) for every file, and another uploads them to a **draft**
   release. Only that last job holds `contents: write`. All actions are pinned by commit SHA.
3. On the laptop: `scripts/sign-release.sh vX.Y.Z`. Before signing, it checks that
   - GitHub's tag points at the same commit as the local tag,
   - the `release.yml` run for that commit and tag succeeded,
   - every file on the release has an attestation from `release.yml` at that tag and commit, on a
     GitHub-hosted runner (`gh attestation verify`),
   - the release carries nothing else, and this machine's binary runs and reports `qh X.Y.Z`.

   Then it writes and signs `qh-release.json`, uploads it, and publishes the draft. Until then the
   release isn't `latest`, so neither the installers nor `qh update` see it.
   `QH_DRY_RUN=1` does everything except upload/publish. `QH_SKIP_PROVENANCE=1` is only for
   re-signing releases built before attestations existed (v0.2.8 and older).
4. If `install.sh` or `install.ps1` changed: `scripts/sync-hosted-installers.sh` (shows the diff),
   then `--push`. The one-line installs are served from the Pages repo `kesavsabari/qh`
   (`sh` and `index.html`), not from this repo.

**What this does and doesn't prove.** The signature says "the maintainer checked this was built by
our workflow from this tag". It does not prove the tagged code is benign, and it isn't a rebuild:
the builds aren't byte-for-byte reproducible yet (the toolchain isn't pinned and build paths are
embedded), so a local rebuild of the Linux binary would not hash the same. Pinning the toolchain
(`rust-toolchain.toml`) plus `--remap-path-prefix` is the start of that, if it's ever wanted.

**The installers** check size and SHA-256 against `qh-release.json` and refuse a mismatch. They check
the manifest's signature only where an OpenSSL 3 exists (most Linux; macOS needs
`brew install openssl@3`, as `/usr/bin/openssl` is LibreSSL; Windows uses Git for Windows' copy).
Otherwise they warn, because the hash alone only proves the binary matches the manifest next to it.
`QH_REQUIRE_SIGNATURE=1` makes them refuse instead. `qh update` always checks the signature.

## Settings to change on GitHub (owner)

- [ ] **2FA** on the GitHub account (passkey or app, not SMS). The `gh` token on the laptop can
      upload release assets, so treat it like the key.
- [ ] **Ruleset for `main`** (Settings → Rules → Rulesets → New branch ruleset, target `main`):
      block force pushes, restrict deletions. Optional: require signed commits.
- [ ] **Ruleset for tags `v*`** (New tag ruleset, target pattern `v*`): restrict creations, updates
      and deletions, bypass list = Repository admin only. A tag can then never be moved to other code
      after it was signed.
- [ ] **Same two rulesets on `kesavsabari/qh`.** That repo *is* the install command: whoever can push
      `sh` or `index.html` there runs code on every new user's machine.
- [ ] **Actions → General → Actions permissions**: "Allow kesavsabari, and select non-kesavsabari,
      actions": allow GitHub-made actions plus `dtolnay/rust-toolchain@*`, `Swatinem/rust-cache@*`,
      `softprops/action-gh-release@*`; tick **Require actions to be pinned to a full-length commit
      SHA**.
- [ ] **Actions → General → Workflow permissions**: "Read repository contents and packages
      permissions"; untick "Allow GitHub Actions to create and approve pull requests".
- [ ] **Fork pull request workflows**: require approval for all outside collaborators
      (`sandbox.yml` runs on `pull_request`; no workflow uses `pull_request_target`).
- [ ] Optional: Dependabot for `github-actions` (`.github/dependabot.yml`) so pinned SHAs get update PRs.
      Re-pin by hand otherwise: `git ls-remote https://github.com/<owner>/<repo> 'refs/tags/<tag>^{}'`.

## Encrypt the release key

`~/.config/qh-release/key.pem` is an unencrypted PEM: anything running as you can copy it and sign
releases that every qh install will accept. Encrypt it (sign-release.sh asks for the passphrase
once, on the terminal, and gives it to openssl on stdin):

```sh
cd ~/.config/qh-release
openssl pkcs8 -topk8 -v2 aes-256-cbc -iter 600000 -in key.pem -out key.enc.pem   # asks for a passphrase
openssl pkey -in key.enc.pem -pubout | openssl pkey -pubin -noout -text           # same public key? compare with key.pem
QH_RELEASE_KEY=$PWD/key.enc.pem QH_DRY_RUN=1 QH_SKIP_PROVENANCE=1 ~/Work/qh-rs/scripts/sign-release.sh v0.2.8
mv key.enc.pem key.pem && chmod 600 key.pem
```

Then delete the old plaintext copy and any backup of it (`shred -u` on a copy; on Btrfs/SSD shred
isn't a guarantee, so assume an old plaintext backup may survive and keep the device trusted). Keep
one encrypted backup offline. A hardware key is stronger still, but changing the key means changing
`PUBLIC_KEY` in `src/update.rs`, so existing installs would reject releases signed by the new key
until a release signed with the old key has shipped the new public key.
