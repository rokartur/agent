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
  want="$(curl -fsSL "https://github.com/rokartur/agent/releases/latest/download/SHA256SUMS" | awk -v name="agent-$os-$arch" '$2 == name { print $1 }')"
  if command -v sha256sum >/dev/null 2>&1; then got="$(sha256sum "$staged" | cut -d' ' -f1)"; else got="$(shasum -a 256 "$staged" | cut -d' ' -f1)"; fi
  if [ -z "$want" ] || [ "$want" != "$got" ]; then
    echo "agent: the download does not match its published SHA-256." >&2
    exit 1
  fi
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
