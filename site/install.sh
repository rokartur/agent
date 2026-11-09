#!/bin/sh
# Installs the latest agent release: curl -fsSL https://agent.rokartur.com/install.sh | sh
set -eu

main() {
  case "$(uname -s)" in
    Darwin) os=macos ;;
    Linux) os=linux ;;
    *) echo "agent: this script installs on macOS and Linux; on Windows download it from https://github.com/rokartur/agent/releases/latest." >&2; exit 1 ;;
  esac
  case "$(uname -m)" in
    arm64 | aarch64) arch=aarch64 ;;
    x86_64) arch=x86_64 ;;
    *) echo "agent: unsupported CPU $(uname -m)." >&2; exit 1 ;;
  esac

  dir="${AGENT_INSTALL_DIR:-$HOME/.local/bin}"
  mkdir -p "$dir"
  staged="$dir/.agent-install-$$"
  trap 'rm -f "$staged"' EXIT
  curl -fsSL "https://github.com/rokartur/agent/releases/latest/download/agent-$os-$arch" -o "$staged"
  chmod 755 "$staged"
  mv "$staged" "$dir/agent"
  echo "Installed $("$dir/agent" --version) to $dir/agent"

  case ":$PATH:" in
    *":$dir:"*) ;;
    *) echo "Add it to PATH: export PATH=\"$dir:\$PATH\"" ;;
  esac
}

# Runs only once the whole script has arrived, so a cut-off download does nothing.
main
