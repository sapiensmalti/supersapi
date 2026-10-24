# Isolation model

The swarm has one rule: **a child can never do more than its parent.** This page defines "more".

## Profiles

Every node — the root included — runs under a profile:

| Field | Values | Ceiling comes from |
|---|---|---|
| directory | a host path | the launcher's working directory |
| access | `read_write` > `read_only` > `none` | `SUPERSAPI_ACCESS` |
| network | `full` > `none` | `SUPERSAPI_NETWORK` |
| timeout | seconds | `SUPERSAPI_TIMEOUT` |
| cpus, memory | docker `--cpus`, `--memory` | `SUPERSAPI_CPUS`, `SUPERSAPI_MEMORY_MIB` |
| depth | 0 for root | `SUPERSAPI_MAX_DEPTH` bounds it |

A `spawn` request may set any field. Unset fields inherit. Set fields are checked against the caller's profile by `policy::derive_child` and the request is refused — with `policy_violation` or `limit_exceeded` — unless every one of these holds:

1. **Directory**: `dir` is relative, exists, is a directory, and its canonical path (symlinks resolved) starts with the caller's canonical directory. Absolute paths and `..` escapes are refused. With `access: none` the directory is irrelevant and ignored.
2. **Access**: `child.access ≤ parent.access`.
3. **Network**: `child.network ≤ parent.network`.
4. **Timeout, cpus, memory**: `child ≤ parent`, and sane (`timeout > 0`, `cpus > 0` finite, `memory ≥ 64 MiB`).
5. **Depth**: `parent.depth + 1 ≤ max_depth`.
6. **Fan-out**: the caller has fewer than `max_children` running children.
7. **Population**: the swarm has fewer than `max_total` running nodes.

There is no privileged caller, no override flag, no "trusted" child. The root's profile is the global ceiling because the root is a node too.

## What each access level means

| access | `/workspace` in the child | Use for |
|---|---|---|
| `read_write` | bind mount of the directory, rw | implementers |
| `read_only` | bind mount, `:ro` | scouts, reviewers, anything whose output is a *report* |
| `none` | anonymous volume, empty, discarded with the container | pure computation, untrusted code, "think about this" |

`read_only` children can still write to `/tmp` and their home; they cannot change the project.

## Visibility

A node sees only its descendants. `list` returns them; `status`, `wait`, `logs`, `stop` on any other id return `forbidden` (or `not_found`). A node cannot see its parent, its siblings, or the root. `shutdown` is root-only.

Children learn nothing about the host: `NodeInfo.dir` is relative to the swarm root, never an absolute host path.

## What is shared

- **Credentials.** All nodes mount the same pi config volume (`sapi-config`), because every node must reach a model provider. A child can therefore use the same provider accounts as the root. If you need a child with *different* credentials, that is a different swarm.
- **The directory tree below the child's directory.** Two children given the same directory can conflict. The agent is told this in the `swarm` tool description; the usual remedy is one directory per child.

## What is not shared

Host environment, SSH agent, other directories, the Docker socket, other containers, process namespaces, the broker's token store. Children have no way to send messages to each other except through files in a shared directory.

## Container hardening

Identical to sapi, applied to every child by [`runtime.rs`](../sapid/src/runtime.rs):

```
--rm --init --pids-limit 1024 --cpus N --memory Nm
--security-opt no-new-privileges
--cap-drop ALL --cap-add CHOWN DAC_OVERRIDE DAC_READ_SEARCH FOWNER SETUID SETGID SETPCAP
```

The seven capabilities exist so the entrypoint can fix volume ownership and drop to the host uid; after `setpriv` the agent process holds none. On Linux children run as *your* uid/gid.

## Limits of the model

- **Not a VM.** Containers share the kernel. For hostile code use a VM-backed Docker (OrbStack, Docker Desktop, or a dedicated VM) — then the kernel in question is the VM's.
- **Network is binary.** `full` or `none`. Egress filtering is a job for the network, not for sapid.
- **Time budgets are wall-clock**, not tokens or dollars. Usage is *reported* per node; cost caps are not enforced by the broker.
- **The agent decides.** The broker guarantees a child cannot exceed its parent. It does not decide whether spawning a child was wise.
