#!/bin/sh
# Copy this repo's installers to the site that serves the one-line installs:
#   kesavsabari.github.io/qh/sh  <- install.sh    (curl -fsSL kesavsabari.github.io/qh/sh | sh)
#   kesavsabari.github.io/qh/    <- install.ps1   (irm kesavsabari.github.io/qh | iex; served as index.html)
# The site is the GitHub Pages repo kesavsabari/qh (root of main). Without --push this clones it into
# a temp folder, copies the files and shows the diff; with --push it also commits and pushes.
# Run it after every change to install.sh / install.ps1, or the hosted copies drift (they had: the
# hosted sh lacked --app and both lacked the release check).
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
site=kesavsabari/qh
d=$(mktemp -d)
trap 'rm -rf "$d"' EXIT
git clone -q --depth 1 "https://github.com/$site.git" "$d/site"
cp "$here/install.sh" "$d/site/sh"
cp "$here/install.ps1" "$d/site/index.html"
cd "$d/site"
if git diff --quiet; then echo "hosted installers already match this repo"; exit 0; fi
git --no-pager diff --stat
if [ "${1:-}" != --push ]; then
  echo "differs from $site; rerun with --push to publish it"
  exit 1
fi
git add sh index.html
git commit -q -m "installers: sync from qh-rs $(git -C "$here" rev-parse --short HEAD)"
git push -q origin HEAD
echo "pushed; GitHub Pages serves it within a minute or two. Check:"
echo "  curl -fsSL https://kesavsabari.github.io/qh/sh | cmp - $here/install.sh"
