#!/usr/bin/env bash
# supersapi installer.
#   curl -fsSL https://raw.githubusercontent.com/sapiensmalti/supersapi/main/install.sh | bash
#
# Installs, and nothing more:
#   ~/.local/bin/supersapi            launcher (bash)
#   ~/.local/bin/sapid                broker (static binary for your OS/arch)
#   ~/.local/share/supersapi/         image/Dockerfile and extension/ (the image sources)
# No sudo. No shell-profile edits. No Docker installation. Override the
# directories with SUPERSAPI_INSTALL_DIR and SUPERSAPI_HOME.
set -Eeuo pipefail

REPO="${SUPERSAPI_REPO:-sapiensmalti/supersapi}"
VERSION="${SUPERSAPI_VERSION:-latest}"
BIN_DIR="${SUPERSAPI_INSTALL_DIR:-$HOME/.local/bin}"
HOME_DIR="${SUPERSAPI_HOME:-$HOME/.local/share/supersapi}"

die() { printf 'supersapi install: %s\n' "$*" >&2; exit 1; }

os="$(uname -s)"; arch="$(uname -m)"
case "$os" in
    Darwin) os_t="apple-darwin" ;;
    Linux)  os_t="unknown-linux-musl" ;;
    *) die "unsupported OS: $os (macOS and Linux are supported)" ;;
esac
case "$arch" in
    x86_64|amd64)  arch_t="x86_64" ;;
    arm64|aarch64) arch_t="aarch64" ;;
    *) die "unsupported architecture: $arch" ;;
esac
target="${arch_t}-${os_t}"
asset="supersapi-${target}.tar.gz"

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v tar  >/dev/null 2>&1 || die "tar is required"
command -v jq   >/dev/null 2>&1 || printf 'supersapi install: note: jq is required at runtime (brew install jq / apt-get install jq)\n' >&2

if [[ "$VERSION" == "latest" ]]; then
    base="https://github.com/$REPO/releases/latest/download"
else
    base="https://github.com/$REPO/releases/download/$VERSION"
fi
# Local/CI override: install from a directory containing the asset and SHA256SUMS.
if [[ -n "${SUPERSAPI_ASSET_DIR:-}" ]]; then base="file://$SUPERSAPI_ASSET_DIR"; fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
fetch() { curl -fsSL --proto '=https,file' --tlsv1.2 "$1" -o "$2" || die "download failed: $1"; }

fetch "$base/$asset" "$tmp/$asset"
fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS"
expected="$(awk -v a="$asset" '$2 == a || $2 == "*"a {print $1}' "$tmp/SHA256SUMS")"
[[ -n "$expected" ]] || die "no checksum for $asset in SHA256SUMS"
if command -v sha256sum >/dev/null 2>&1; then actual="$(sha256sum "$tmp/$asset" | awk '{print $1}')"
else actual="$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')"; fi
[[ "$actual" == "$expected" ]] || die "checksum mismatch for $asset"

tar -xzf "$tmp/$asset" -C "$tmp"
src="$tmp/supersapi-$target"
[[ -x "$src/sapid" && -f "$src/supersapi" && -f "$src/image/Dockerfile" && -f "$src/extension/index.ts" ]] \
    || die "archive is missing files"
bash -n "$src/supersapi" || die "launcher failed a syntax check"
"$src/sapid" --version >/dev/null 2>&1 || die "sapid binary does not run on this machine"

mkdir -p "$BIN_DIR" "$HOME_DIR"
install -m 0755 "$src/sapid" "$BIN_DIR/sapid"
install -m 0755 "$src/supersapi" "$BIN_DIR/supersapi"
rm -rf "$HOME_DIR/image" "$HOME_DIR/extension"
cp -R "$src/image" "$src/extension" "$HOME_DIR/"

printf 'supersapi install: installed %s (v%s), %s (%s)\n' \
    "$BIN_DIR/supersapi" "$("$BIN_DIR/supersapi" version)" "$BIN_DIR/sapid" "$("$BIN_DIR/sapid" --version)"

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) printf '\n  %s is not on your PATH. Add this to your shell profile:\n\n    export PATH="%s:$PATH"\n' "$BIN_DIR" "$BIN_DIR" ;;
esac
if ! command -v docker >/dev/null 2>&1; then
    cat <<'EOF'

  Docker is not installed. supersapi needs it for isolation.
    macOS:   brew install orbstack        (or Docker Desktop)
    Ubuntu:  sudo apt-get install -y docker.io && sudo usermod -aG docker "$USER"
             then log out and back in.
EOF
fi
printf '\n  Run `supersapi doctor`, then `supersapi` in any project directory.\n\n'
