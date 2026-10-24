#!/usr/bin/env bash
# A stand-in for `docker` used by the integration tests.
#
#   info            → ok
#   run ... IMAGE ... -- TASK
#                   → behaves like a pi child in --mode json:
#                       "echo:<text>"   print an assistant message with <text>, exit 0
#                       "sleep:<secs>"  sleep, then echo, exit 0
#                       "fail"          print to stderr, exit 3
#                       "grandchild:<task>"  act as a client: spawn a grandchild with <task>
#                                             (and the given SPAWN_ARGS env), wait, print its output
#                       "escape"        try to spawn a grandchild with dir ".." and print the error code
#   stop NAME       → kill the run process registered under NAME
set -eu

FAKE_STATE="${FAKE_DOCKER_STATE:?}"
mkdir -p "$FAKE_STATE"
log() { printf '%s\n' "$*" >>"$FAKE_STATE/calls.log"; }
log "$*"

assistant() {
    # Shape mirrors pi's message_end for an assistant message.
    printf '{"type":"message_end","message":{"role":"assistant","model":"fake-model","content":[{"type":"text","text":"%s"}],"usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":15,"cost":{"total":0.001}}}}\n' "$1"
}

case "${1:-}" in
    info) exit 0 ;;
    stop)
        name="${*: -1}"
        if [ -f "$FAKE_STATE/$name.pid" ]; then
            pid="$(cat "$FAKE_STATE/$name.pid")"
            pkill -P "$pid" 2>/dev/null || true
            kill "$pid" 2>/dev/null || true
        fi
        exit 0 ;;
    run) ;;
    *) echo "fake docker: unsupported: $*" >&2; exit 2 ;;
esac

# Extract what we need from the argv.
name=""; node=""; sock=""; task="${*: -1}"; mount_rw=""; network="default"
args=("$@")
for ((i = 0; i < ${#args[@]}; i++)); do
    case "${args[i]}" in
        --name) name="${args[i+1]}" ;;
        --volume)
            case "${args[i+1]}" in
                *:/run/sapid/sapid.sock) sock="${args[i+1]%%:*}" ;;
                *:/workspace|*:/workspace:ro) mount_rw="${args[i+1]}" ;;
            esac ;;
        --network) network="${args[i+1]}" ;;
        --label) case "${args[i+1]}" in sh.supersapi.node=*) node="${args[i+1]#*=}" ;; esac ;;
    esac
done
printf '%s' "$$" >"$FAKE_STATE/$name.pid"
printf 'mount=%s network=%s\n' "$mount_rw" "$network" | tee "$FAKE_STATE/$name.spec" >"$FAKE_STATE/node-$node.spec"
trap 'rm -f "$FAKE_STATE/$name.pid"' EXIT

printf '{"type":"agent_start"}\n'
case "$task" in
    echo:*) assistant "${task#echo:}" ;;
    sleep:*) sleep "${task#sleep:}"; assistant "slept" ;;
    fail) echo "something broke" >&2; exit 3 ;;
    grandchild:*)
        # shellcheck disable=SC2086
        out="$("$SAPID_BIN" ctl --addr "unix://$sock" spawn ${SPAWN_ARGS:-} --wait "${task#grandchild:}" 2>/dev/null | tail -n +1)"
        text="$(printf '%s' "$out" | tr -d '\n' | sed 's/.*"output": *"\([^"]*\)".*/\1/')"
        assistant "grandchild said: $text" ;;
    escape)
        if "$SAPID_BIN" ctl --addr "unix://$sock" spawn --dir .. "x" 2>"$FAKE_STATE/$name.err" >/dev/null; then
            assistant "ESCAPED"
        else
            assistant "refused: $(tr -d '\n' <"$FAKE_STATE/$name.err")"
        fi ;;
    *) assistant "unknown task" ;;
esac
printf '{"type":"agent_end"}\n'
