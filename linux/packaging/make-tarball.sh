#!/bin/sh
# Builds qh-app-linux-x86_64.tar.gz from a built binary: make-tarball.sh <qh-app binary> <out dir>
set -e
here=$(cd "$(dirname "$0")" && pwd)
bin=$1; out=${2:-.}
d=$(mktemp -d)/qh-app-linux-x86_64
install -Dm755 "$bin" "$d/bin/qh-app"
install -Dm644 "$here/dev.qh.App.desktop" "$d/share/applications/dev.qh.App.desktop"
install -Dm644 "$here/dev.qh.App.svg" "$d/share/icons/hicolor/scalable/apps/dev.qh.App.svg"
install -Dm644 "$here/dev.qh.App-128.png" "$d/share/icons/hicolor/128x128/apps/dev.qh.App.png"
install -Dm644 "$here/dev.qh.App-256.png" "$d/share/icons/hicolor/256x256/apps/dev.qh.App.png"
install -Dm644 "$here/dev.qh.App.metainfo.xml" "$d/share/metainfo/dev.qh.App.metainfo.xml"
tar -czf "$out/qh-app-linux-x86_64.tar.gz" -C "$(dirname "$d")" qh-app-linux-x86_64
echo "$out/qh-app-linux-x86_64.tar.gz"
