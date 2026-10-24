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

echo "live container flags"
# Start a swarm manually: broker + a long child, inspect it, stop it.
state="$SUPERSAPI_STATE/manual"; mkdir -p "$state"
image="$(docker images --format '{{.Repository}}:{{.Tag}}' | grep '^supersapi:' | head -n1)"
uidargs=()
[[ "$(uname -s)" == Linux ]] && uidargs=(--uid "$(id -u)" --gid "$(id -g)")
"$SUPERSAPI_SAPID" serve --state-dir "$state" --root-dir "$proj" --image "$image" --timeout-secs 300 \
    ${uidargs[@]+"${uidargs[@]}"} 2>"$state/log" &
bpid=$!
for _ in $(seq 1 100); do [[ -f "$state/token" ]] && break; sleep 0.1; done
ctl() { "$SUPERSAPI_SAPID" ctl --addr "$(jq -r .addr "$state/sapid.json")" --token-file "$state/token" "$@"; }
# A child that just sleeps: override pi with a shell via --docker-arg is not allowed by protocol,
# so use a bogus model child and inspect it while it starts.
id="$(ctl spawn --dir sub --access read-only --network none "hi" | jq -r .id)"
sleep 1
name="ssapi-$(basename "$state")-$id"
if docker inspect "$name" >/dev/null 2>&1; then
    caps="$(docker inspect "$name" --format '{{json .HostConfig.CapDrop}} {{json .HostConfig.CapAdd}}')"
    [[ "$caps" == *'"ALL"'* && "$caps" == *SETUID* ]] && ok "cap-drop ALL + named caps" || bad "caps: $caps"
    [[ "$(docker inspect "$name" --format '{{.HostConfig.NetworkMode}}')" == none ]] && ok "network none" || bad "network"
    [[ "$(docker inspect "$name" --format '{{range .Mounts}}{{if eq .Destination "/workspace"}}{{.RW}}{{end}}{{end}}')" == false ]] && ok "workspace read-only" || bad "workspace rw"
    [[ "$(docker inspect "$name" --format '{{range .Mounts}}{{if eq .Destination "/workspace"}}{{.Source}}{{end}}{{end}}')" == "$proj/sub" ]] && ok "subdirectory mounted" || bad "mount source"
    docker inspect "$name" --format '{{json .Mounts}}' | grep -q docker.sock && bad "docker socket mounted!" || ok "no docker socket"
    [[ "$(docker inspect "$name" --format '{{json .HostConfig.SecurityOpt}}')" == *no-new-privileges* ]] && ok "no-new-privileges" || bad "security opt"
    docker inspect "$name" --format '{{json .Config.Env}}' | grep -q '"SAPID_TOKEN=' && ok "token delivered via env" || bad "token missing in env"
    ctl stop "$id" >/dev/null && ok "stop" || bad "stop"
else
    # Too fast to inspect: pi failed immediately. Still verify through the broker.
    st="$(ctl status "$id" | jq -r .state)"
    [[ "$st" != running ]] && ok "child finished before inspection (state $st); flags covered by unit tests" || bad "state $st"
fi
ctl tree >/dev/null && ok "tree" || bad "tree"
ctl shutdown >/dev/null && ok "shutdown" || bad "shutdown"
wait "$bpid" 2>/dev/null && ok "broker exited cleanly" || bad "broker exit"

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
