#!/usr/bin/env bash
# The real thing, with Docker: build the image, run doctor, spawn a child
# through the broker, verify isolation flags on the live container, stop it.
# Children run pi with a bogus model so no provider is contacted; they exit
# non-zero quickly, which is enough to prove the pipeline.
set -Eeuo pipefail
cd "$(dirname "$0")/.."
pass=0; fail=0
ok()  { pass=$((pass + 1)); printf '  ok   %s\n' "$*"; }
bad() { fail=$((fail + 1)); printf '  FAIL %s\n' "$*" >&2; }
docker info >/dev/null 2>&1 || { echo "docker is not available" >&2; exit 1; }

SUPERSAPI_STATE="$(mktemp -d)"
export SUPERSAPI_HOME="$PWD" SUPERSAPI_STATE SUPERSAPI_SAPID="$PWD/sapid/target/release/sapid"
[[ -x "$SUPERSAPI_SAPID" ]] || (cd sapid && cargo build --release --locked)
proj="$(mktemp -d)"; mkdir -p "$proj/sub"
cleanup() { docker ps -aq --filter label=sh.supersapi.managed=true | xargs -r docker rm -f >/dev/null 2>&1 || true; rm -rf "$proj" "$SUPERSAPI_STATE"; }
trap cleanup EXIT

echo "doctor"
if ./supersapi doctor; then ok "doctor"; else bad "doctor"; fi

echo "headless run"
cd "$proj"
export SAPI_MODEL="bogus/none" SAPI_AUTO_UPDATE=0 SUPERSAPI_TIMEOUT=120
set +e
out="$(timeout 300 "$OLDPWD/supersapi" run "say hi" 2>/tmp/smoke.err)"; code=$?
set -e
# pi cannot find the model → exits non-zero; the broker must report that and the launcher must exit non-zero.
[[ $code -ne 0 ]] && ok "run propagates a failing child (exit $code)" || bad "run should fail with a bogus model: '$out'"
grep -q "spawned 001" /tmp/smoke.err && ok "child was spawned through the broker" || { bad "no spawn"; cat /tmp/smoke.err; }
log="$(ls "$SUPERSAPI_STATE"/*/nodes/001.jsonl 2>/dev/null | head -n1)"
[[ -n "$log" ]] && ok "child log written: $(wc -l <"$log") lines" || bad "no child log"
grep -qi "model" "$log" && ok "child stderr captured in log" || bad "log lacks the pi error"
pgrep -x sapid >/dev/null && bad "broker left running" || ok "broker gone after run"

echo "argv handed to the real docker"
# Wrap docker so we can assert exactly what the broker asked for, then let it run for real.
state="$SUPERSAPI_STATE/manual"; mkdir -p "$state"
printf '#!/usr/bin/env bash\nprintf "%%s\\n" "$*" >> "%s/argv.log"\nexec docker "$@"\n' "$state" >"$state/docker"; chmod +x "$state/docker"
image="$(docker images --format '{{.Repository}}:{{.Tag}}' | grep '^supersapi:' | head -n1)"
uidargs=()
[[ "$(uname -s)" == Linux ]] && uidargs=(--uid "$(id -u)" --gid "$(id -g)")
"$SUPERSAPI_SAPID" serve --state-dir "$state" --root-dir "$proj" --image "$image" --timeout-secs 300 --docker "$state/docker" \
    ${uidargs[@]+"${uidargs[@]}"} 2>"$state/log" &
bpid=$!
for _ in $(seq 1 100); do [[ -f "$state/token" ]] && break; sleep 0.1; done
ctl() { "$SUPERSAPI_SAPID" ctl --addr "$(jq -r .addr "$state/sapid.json")" --token-file "$state/token" "$@"; }
res="$(ctl spawn --dir sub --access read-only --network none --wait "hi")"
[[ "$(printf '%s' "$res" | jq -r 'select(has("node")) | .node.state')" == exited ]] && ok "child ran to completion under real docker" || bad "child: $res"
argv="$(grep '^run ' "$state/argv.log" | head -n1)"
check_argv() { [[ "$argv" == *"$1"* ]] && ok "argv: $1" || bad "argv lacks: $1"; }
check_argv "--rm --init"
check_argv "--cap-drop ALL"
check_argv "--cap-add SETUID"
check_argv "--security-opt no-new-privileges"
check_argv "--network none"
check_argv "$proj/sub:/workspace:ro"
check_argv "--pids-limit 1024"
check_argv "--env SAPID_TOKEN "
[[ "$argv" == *docker.sock* ]] && bad "docker socket in argv!" || ok "argv: no docker socket"
[[ "$argv" == *"SAPID_TOKEN="* ]] && bad "token value in argv!" || ok "argv: token not on the command line"
[[ "$(uname -s)" == Linux ]] && check_argv "SAPI_UID=$(id -u)"
[[ "$(docker ps -aq --filter label=sh.supersapi.managed=true | wc -l)" -eq 0 ]] && ok "no containers left behind (--rm)" || bad "containers left behind"
ctl tree >/dev/null && ok "tree" || bad "tree"
ctl shutdown >/dev/null && ok "shutdown" || bad "shutdown"
wait "$bpid" 2>/dev/null && ok "broker exited cleanly" || bad "broker exit"

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
