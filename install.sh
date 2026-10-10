#!/bin/sh
# qh installer for Linux/macOS:  curl -fsSL https://raw.githubusercontent.com/kesavsabari/qh-rs/main/install.sh | sh
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
curl -fL "https://github.com/$repo/releases/latest/download/$asset" -o "$dir/qh"
chmod +x "$dir/qh"
echo "Installed $dir/qh"
case ":$PATH:" in *":$dir:"*) ;; *) echo "Add $dir to your PATH." ;; esac
echo "Needs a model: Ollama (https://ollama.com) + 'ollama pull qwen3.5:9b', or 'qh login openrouter'."
