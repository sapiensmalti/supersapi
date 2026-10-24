# Broker protocol

Version 1. JSON Lines over a Unix socket (`unix:///run/sapid/sapid.sock` inside containers) or TCP (`tcp://host.docker.internal:PORT`). The address and this node's token arrive in `SAPID_ADDR` and `SAPID_TOKEN`.

## Framing

Client → broker, one request per line:

```json
{"id":"<client id>","token":"<token>","op":"<name>","params":{...}}
```

Broker → client, for each request: zero or more events, then exactly one final frame with the same `id`:

```json
{"id":"1","event":{...}}
{"id":"1","ok":true,"result":{...}}
{"id":"1","ok":false,"error":{"code":"<code>","message":"..."}}
```

Requests on one connection may be pipelined; each is dispatched to its own task and answered independently, so a long `wait` never blocks a `list`. Lines over 1 MiB are refused. Any malformed line yields an error frame with whatever `id` could be recovered.

## Operations

| op | params | result | notes |
|---|---|---|---|
| `hello` | — | `{protocol, server, node, limits}` | identify yourself |
| `spawn` | `SpawnParams` | `{id, container}` | refused if wider than caller |
| `list` | — | `{nodes: [NodeInfo]}` | descendants only, sorted by id |
| `status` | `{id}` | `NodeInfo` | |
| `wait` | `{id, stream?, timeout_secs?}` | `{node, output, timed_out}` | streams events when `stream` |
| `logs` | `{id, follow?}` | `{lines}` | events `{line}` carry the raw JSONL |
| `stop` | `{id}` | `{stopped: [id]}` | stops the subtree, deepest first |
| `shutdown` | — | `{stopped: [id]}` | root only; broker exits |

### SpawnParams

```json
{
  "task": "string, required — the child's prompt, verbatim",
  "dir": "relative path inside the caller's workspace (default \".\")",
  "access": "read_write | read_only | none (default: inherit)",
  "network": "full | none (default: inherit)",
  "model": "pi model id (default: pi decides)",
  "timeout_secs": 3600,
  "cpus": 2.0,
  "memory_mib": 4096,
  "name": "label",
  "approve_project": false
}
```

`approve_project` passes `--approve` to the child's pi so project-local `.pi/` files are trusted; default is `--no-approve`. A directory controlled by someone else can contain extensions — leave this off unless you own the tree.

### NodeInfo

```json
{
  "id": "001", "parent": "000", "depth": 1,
  "name": "api", "task": "first line of the task…",
  "dir": "services/api", "access": "read_write", "network": "full", "timeout_secs": 3600,
  "state": "running | exited | stopped | failed",
  "exit_code": 0, "reason": "timeout | stopped by 000 | shutdown | docker run: …",
  "started_at": 1761340000, "finished_at": 1761340100,
  "usage": {"turns": 3, "input": 12000, "output": 800, "cache_read": 0, "cache_write": 0, "cost": 0.04},
  "model": "anthropic/claude-…"
}
```

`dir` is relative to the swarm root. Host paths are never sent.

### Streaming `wait` events

Filtered from the child's pi JSON stream so a parent sees what matters:

```json
{"type":"assistant","text":"…"}
{"type":"tool_execution_start","tool":"bash","args":{"command":"…"}}
{"type":"tool_execution_end","tool":"bash","is_error":false}
{"type":"stderr","line":"…"}
{"type":"lagged","skipped":12}
```

`lagged` means the client fell more than 4096 lines behind and some were skipped; the complete log is still on disk and available through `logs`.

## Error codes

| code | meaning |
|---|---|
| `bad_request` | malformed request, empty task |
| `unauthorized` | unknown token |
| `forbidden` | node exists but is not your descendant; or `shutdown` from a non-root |
| `not_found` | no such node |
| `policy_violation` | spawn would widen access, escape the directory, or use nonsense values |
| `limit_exceeded` | depth, fan-out, or population limit |
| `runtime_error` | `docker run` failed to start |
| `shutting_down` | broker is exiting |

## Example session

```
→ {"id":"1","token":"…","op":"hello"}
← {"id":"1","ok":true,"result":{"protocol":1,"server":"0.1.0","node":{"id":"000",…},"limits":{"max_children":4,"max_depth":2,"max_total":16}}}
→ {"id":"2","token":"…","op":"spawn","params":{"task":"List every TODO in src/ with file:line.","dir":"src","access":"read_only","name":"todos"}}
← {"id":"2","ok":true,"result":{"id":"001","container":"ssapi-9f2a1c3e-001"}}
→ {"id":"3","token":"…","op":"wait","params":{"id":"001","stream":true}}
← {"id":"3","event":{"type":"tool_execution_start","tool":"bash","args":{"command":"rg -n TODO"}}}
← {"id":"3","event":{"type":"assistant","text":"Found 14 TODOs:\n…"}}
← {"id":"3","ok":true,"result":{"node":{"id":"001","state":"exited","exit_code":0,…},"output":"Found 14 TODOs:\n…","timed_out":false}}
```

The same conversation from a shell: `sapid ctl --addr … --token … spawn --dir src --access read-only --wait "List every TODO…"`.
