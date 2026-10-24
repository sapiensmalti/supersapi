#!/usr/bin/env bash
# Everything that can be verified without Docker: Rust lints and tests, the
# extension against a real broker, the installer, and the launcher through a
# docker shim. Requires: cargo, node ≥ 22, jq. Optional: shellcheck.
set -Eeuo pipefail
cd "$(dirname "$0")/.."
pass=0; fail=0
ok()  { pass=$((pass + 1)); printf '  ok   %s\n' "$*"; }
bad() { fail=$((fail + 1)); printf '  FAIL %s\n' "$*" >&2; }
check() { local msg="$1"; shift; if "$@" >/tmp/check.out 2>&1; then ok "$msg"; else bad "$msg"; tail -n 20 /tmp/check.out >&2; fi; }

echo "shell"
check "launcher parses"   bash -n supersapi
check "installer parses"  bash -n install.sh
check "package.sh parses" bash -n scripts/package.sh
if command -v shellcheck >/dev/null 2>&1; then
    check "shellcheck" shellcheck -S warning supersapi install.sh scripts/package.sh test/*.sh sapid/tests/fake-docker.sh
else echo "  skip shellcheck (not installed)"; fi
check "launcher version" ./supersapi version
check "launcher help"    ./supersapi help

echo "rust"
( cd sapid
  check "cargo fmt"    cargo fmt --check
  check "cargo clippy" cargo clippy --all-targets --locked -- -D warnings
  check "cargo test"   cargo test --locked
  check "cargo doc"    env RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked
  check "release build" cargo build --release --locked
)
SAPID=sapid/target/release/sapid

echo "extension"
if [[ ! -e node_modules ]]; then
    pi_mods="$(npm root -g 2>/dev/null)/@earendil-works/pi-coding-agent/node_modules"
    [[ -d "$pi_mods" ]] && ln -s "$pi_mods" node_modules && trap 'rm -f node_modules' EXIT
fi
if [[ -d node_modules/typebox ]]; then
    check "extension harness (real broker, fake docker)" node test/extension-harness.mjs "$SAPID"
else echo "  skip extension harness (typebox not resolvable; install pi globally)"; fi

echo "installer"
assets="$(mktemp -d)"; home="$(mktemp -d)"; bin="$(mktemp -d)"
check "package"  scripts/package.sh test-target "$SAPID" "$assets"
check "install from assets" env SUPERSAPI_ASSET_DIR="$assets" SUPERSAPI_INSTALL_DIR="$bin" SUPERSAPI_HOME="$home" \
    bash -c 'sed -e "s/^target=.*/target=test-target/" -e "s/^asset=.*/asset=supersapi-test-target.tar.gz/" install.sh | bash'
check "installed files" test -x "$bin/supersapi" -a -x "$bin/sapid" -a -f "$home/image/Dockerfile" -a -f "$home/extension/index.ts"
sed -i.bak 's/^./x/' "$assets/SHA256SUMS"   # 'x' is never a hex digit
check "tampered checksum refused" bash -c "! SUPERSAPI_ASSET_DIR=$assets SUPERSAPI_INSTALL_DIR=$(mktemp -d) bash -c 'sed -e \"s/^target=.*/target=test-target/\" -e \"s/^asset=.*/asset=supersapi-test-target.tar.gz/\" install.sh | bash'"

echo "launcher (docker shim)"
shim="$(mktemp -d)"
cat >"$shim/docker" <<SHIM
#!/usr/bin/env bash
case "\$1" in
  info) [[ "\${2:-}" == --format ]] && echo "Shim Linux"; exit 0 ;;
  image|rm) exit 0 ;;
  volume|network) echo x ;;
  run|stop) exec "$PWD/sapid/tests/fake-docker.sh" "\$@" ;;
  *) exit 1 ;;
esac
SHIM
chmod +x "$shim/docker"
proj="$(mktemp -d)"; state="$(mktemp -d)"; fake="$(mktemp -d)"
out="$(cd "$proj" && PATH="$shim:$PATH" FAKE_DOCKER_STATE="$fake" SAPID_BIN="$bin/sapid" SUPERSAPI_HOME="$home" SUPERSAPI_STATE="$state" \
    timeout 60 "$bin/supersapi" run "echo:launcher works" 2>/dev/null || true)"
[[ "$out" == "launcher works" ]] && ok "supersapi run (headless) prints the child's output" || bad "supersapi run: got '$out'"
pgrep -x sapid >/dev/null && bad "broker left running" || ok "broker exits with the launcher"
set +e
(cd "$proj" && PATH="$shim:$PATH" FAKE_DOCKER_STATE="$fake" SAPID_BIN="$bin/sapid" SUPERSAPI_HOME="$home" SUPERSAPI_STATE="$state" \
    timeout 60 "$bin/supersapi" run "fail" >/dev/null 2>&1); code=$?
set -e
[[ "$code" -eq 3 ]] && ok "supersapi run propagates the child's exit code (3)" || bad "run exit code: $code"
out="$(cd "$proj" && SUPERSAPI_STATE="$state" "$bin/supersapi" ps 2>&1 || true)"
[[ "$out" == *"no swarm is running"* ]] && ok "ps without a swarm" || bad "ps: $out"

rm -rf "$assets" "$home" "$bin" "$shim" "$proj" "$state" "$fake"
printf '\n%d passed, %d failed\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
