#!/usr/bin/env bash
# Package a release asset: supersapi-<target>.tar.gz containing the launcher,
# the sapid binary, and the image sources. Appends to SHA256SUMS.
# Usage: scripts/package.sh <target> <path-to-sapid-binary> <out-dir>
set -Eeuo pipefail
cd "$(dirname "$0")/.."
target="$1"; sapid_bin="$2"; out="$3"
name="supersapi-$target"
stage="$(mktemp -d)"; trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name/image" "$stage/$name/extension" "$out"
install -m 0755 "$sapid_bin" "$stage/$name/sapid"
install -m 0755 supersapi "$stage/$name/supersapi"
cp image/Dockerfile "$stage/$name/image/"
cp extension/*.ts "$stage/$name/extension/"
cp LICENSE "$stage/$name/"
tar -C "$stage" -czf "$out/$name.tar.gz" "$name"
( cd "$out" && if command -v sha256sum >/dev/null 2>&1; then sha256sum "$name.tar.gz"; else shasum -a 256 "$name.tar.gz"; fi >> SHA256SUMS )
printf 'packaged %s/%s.tar.gz\n' "$out" "$name"
