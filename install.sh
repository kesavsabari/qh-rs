#!/bin/sh
# qh installer for Linux/macOS:  curl -fsSL kesavsabari.github.io/qh/sh | sh
set -e
repo=kesavsabari/qh-rs
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

echo
if [ "$ready" = 1 ]; then
  echo "Run: qh"
else
  echo "Open a new terminal and run: qh"
  echo "(or in this one: export PATH=\"\$HOME/.local/bin:\$PATH\" && qh)"
fi
echo "Needs a model: Ollama (https://ollama.com) + 'ollama pull qwen3.5:9b', or 'qh login openrouter'."
