#!/bin/sh
# qh installer for Linux/macOS:  curl -fsSL kesavsabari.github.io/qh/sh | sh
set -e
repo=kesavsabari/qh-rs
# opt-in desktop app (Linux): QH_APP=1 curl ... | sh   or   curl ... | sh -s -- --app
for a in "$@"; do [ "$a" = --app ] && QH_APP=1; done
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)  asset=qh-linux-x86_64 ;;
  Darwin-arm64)  asset=qh-macos-arm64 ;;
  Darwin-x86_64) asset=qh-macos-x86_64 ;;
  *) echo "no prebuilt binary for $(uname -s) $(uname -m); build from source with cargo"; exit 1 ;;
esac
dir="$HOME/.local/bin"
mkdir -p "$dir"
echo "Downloading qh..."
curl -fsSL "https://github.com/$repo/releases/latest/download/$asset" -o "$dir/qh.tmp"
chmod +x "$dir/qh.tmp"
mv "$dir/qh.tmp" "$dir/qh"
echo "Installed $dir/qh"

# Put ~/.local/bin on PATH for new terminals (macOS doesn't have it by default).
case ":$PATH:" in
  *":$dir:"*) ready=1 ;;
  *)
    ready=0
    line='export PATH="$HOME/.local/bin:$PATH"'
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

# the desktop app, only when asked (QH_APP=1 or --app); QH_APP_TARBALL=/path installs a local tarball (testing)
if [ "${QH_APP:-0}" = 1 ]; then
  if [ "$(uname -s)-$(uname -m)" != Linux-x86_64 ]; then
    echo "qh-app: no prebuilt app for $(uname -s) $(uname -m); skipped"
  else
    tmp=$(mktemp -d)
    if [ -n "$QH_APP_TARBALL" ]; then cp "$QH_APP_TARBALL" "$tmp/app.tar.gz"
    else echo "Downloading qh-app..."
      curl -fsSL "https://github.com/$repo/releases/latest/download/qh-app-linux-x86_64.tar.gz" -o "$tmp/app.tar.gz"; fi
    tar -xzf "$tmp/app.tar.gz" -C "$tmp"
    src="$tmp/qh-app-linux-x86_64"
    share="$HOME/.local/share"
    install -Dm755 "$src/bin/qh-app" "$dir/qh-app"
    install -Dm644 "$src/share/applications/dev.qh.App.desktop" "$share/applications/dev.qh.App.desktop"
    sed -i "s|^Exec=qh-app|Exec=$dir/qh-app|" "$share/applications/dev.qh.App.desktop"
    install -Dm644 "$src/share/icons/hicolor/scalable/apps/dev.qh.App.svg" "$share/icons/hicolor/scalable/apps/dev.qh.App.svg"
    for n in 128 256; do
      f="$src/share/icons/hicolor/${n}x${n}/apps/dev.qh.App.png"
      [ -f "$f" ] && install -Dm644 "$f" "$share/icons/hicolor/${n}x${n}/apps/dev.qh.App.png"
    done
    install -Dm644 "$src/share/metainfo/dev.qh.App.metainfo.xml" "$share/metainfo/dev.qh.App.metainfo.xml"
    rm -rf "$tmp"
    command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q "$share/applications" || true
    command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -qtf "$share/icons/hicolor" || true
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
