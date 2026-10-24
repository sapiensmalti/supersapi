# supersapi

**A swarm of [sapi](https://github.com/sapiensmalti/sapi)s. Extremely dependable, relentlessly resourceful.**

[![ci](https://github.com/sapiensmalti/supersapi/actions/workflows/ci.yml/badge.svg)](https://github.com/sapiensmalti/supersapi/actions/workflows/ci.yml)

```sh
curl -fsSL https://raw.githubusercontent.com/sapiensmalti/supersapi/main/install.sh | bash
```

```sh
cd your/project
supersapi                      # an agent that can spawn agents
supersapi run "audit every package in packages/ for unused dependencies and fix them"
```

One agent becomes many. Each child runs in its own container, with its own context window, and **can never do more than its parent**: a narrower directory, the same or less filesystem access, the same or less network, a smaller time budget. The Docker socket never enters a container.

---

## What it is

`sapi` runs one agent in one container scoped to one directory. `supersapi` adds a broker on the host — `sapid`, a small Rust daemon — that lets that agent ask for more agents:

```
host                                            containers
┌────────────────────────────────┐              ┌──────────────────────────────┐
│ supersapi (launcher)           │              │ 000  root agent  /workspace  │
│   └─ sapid (broker)  ◀─────────┼── socket ────┤      swarm({tasks:[…]})      │
│        │  spawn / wait / logs  │              ├──────────────────────────────┤
│        │  stop / list / hello  │              │ 001  child   /workspace/api  │
│        ▼                       │              │ 002  child   /workspace/web  │
│      docker run  (one per      │              │ 003  child   /workspace  :ro │
│      child, hardened, --rm)    │              │   └ 004  grandchild …        │
└────────────────────────────────┘              └──────────────────────────────┘
```

The agent sees three tools — `swarm`, `swarm_ps`, `swarm_stop` — and a `/swarm` command. It writes task briefs; the broker does the rest. Children may themselves call `swarm`, down to a configurable depth.

## Why not just mount the Docker socket?

Because `/var/run/docker.sock` *is* root on the host. A child that can talk to Docker can mount `/`, read your keys, and run anything — the whole point of the sandbox evaporates in one `-v`. `sapid` exposes nine verbs instead, and the only thing a node can do with them is create something *smaller than itself*. See [docs/security.md](docs/security.md).

## Install

Requirements: macOS or Linux, Docker ([OrbStack](https://orbstack.dev) or Docker Desktop on macOS, `docker.io` on Ubuntu), `curl`, `jq`.

```sh
curl -fsSL https://raw.githubusercontent.com/sapiensmalti/supersapi/main/install.sh | bash
supersapi doctor
```

The installer downloads one release archive for your OS/arch, verifies its checksum, and places `supersapi` and `sapid` in `~/.local/bin` and the image sources in `~/.local/share/supersapi`. No sudo, no profile edits. `doctor` builds the image and proves a container can reach the broker on *your* Docker setup.

Prefer to read first: `curl -fsSL …/install.sh -o install.sh && less install.sh && bash install.sh`.

**Uninstall:** `rm ~/.local/bin/{supersapi,sapid} && rm -rf ~/.local/share/supersapi ~/.local/state/supersapi`.

## Use

```
supersapi              interactive root agent in the current directory
supersapi run TASK     headless: one agent, one task, final message on stdout, exit code
supersapi ps           the swarm tree for this directory
supersapi logs ID      a node's full JSONL log
supersapi stop ID      stop a node and everything below it
supersapi doctor       check Docker, image, and container → broker connectivity
```

Inside the root agent, just ask:

> Split the migration across the three services in `services/`. Run one child per service; each should update its own package, run its tests, and report what changed. Use a read-only scout first to list every call site.

The agent calls `swarm` with one task per child. You watch them stream in the TUI. `/swarm` shows the tree at any time.

### Configure the ceiling

Everything is an environment variable on the launcher. These are **ceilings**: the root gets them, children get at most them.

| Variable | Default | Meaning |
|---|---|---|
| `SUPERSAPI_ACCESS` | `read-write` | `read-only`, or `none` (scratch volume, no workspace) |
| `SUPERSAPI_NETWORK` | `full` | `none` = loopback only |
| `SUPERSAPI_TIMEOUT` | `3600` | seconds per child |
| `SUPERSAPI_CPUS` / `SUPERSAPI_MEMORY_MIB` | `2` / `4096` | per child |
| `SUPERSAPI_MAX_CHILDREN` | `4` | running children per node |
| `SUPERSAPI_MAX_DEPTH` | `2` | root → child → grandchild |
| `SUPERSAPI_MAX_TOTAL` | `16` | running nodes in the swarm |
| `SAPI_MODEL` | *(pi decides)* | root's model; children choose per task |
| `SAPI_ENV` | | host variables passed to the root, e.g. `"ANTHROPIC_API_KEY"` |
| `SAPI_CONFIG_VOLUME` | `sapi-config` | pi credentials volume, shared with `sapi` |
| `SUPERSAPI_TRANSPORT` | auto | `unix` or `tcp` (Docker Desktop needs `tcp`; auto-detected) |

Per child, the agent can narrow further in the `swarm` call: `dir`, `access`, `network`, `model`, `timeout_secs`, `cpus`, `memory_mib`. Anything wider than the caller is refused with `policy_violation`.

## How it holds together

- **One broker per swarm**, started by the launcher, dies with it, and takes every child down when it goes (`--exit-with-pid`).
- **Tokens, not trust.** Every node has a random 256-bit token bound to its profile. The broker checks the token on every request and only ever shows a node its own descendants.
- **Attenuation is one pure function** ([`policy.rs`](sapid/src/policy.rs)) with no I/O except canonicalising paths — so symlink and `..` escapes are caught — and it is unit-tested against every rule.
- **Docker via the CLI.** The full `docker run` argv for a child is built in one function ([`runtime.rs`](sapid/src/runtime.rs)) and asserted in tests: `--rm --init --cap-drop ALL`, seven named caps, `no-new-privileges`, `--pids-limit`, cpu/memory limits, exactly one bind mount. The token never appears on the command line.
- **Everything is logged.** Each child's complete JSONL stream is written to `~/.local/state/supersapi/<swarm>/nodes/<id>.jsonl` and readable live through the protocol.
- **No Docker needed to test it.** The broker is exercised end to end by [a fake `docker`](sapid/tests/fake-docker.sh); CI then runs the real thing on Ubuntu.

More: [architecture](docs/architecture.md) · [isolation model](docs/isolation.md) · [protocol](docs/protocol.md) · [security](docs/security.md) · [operations & debugging](docs/operations.md)

## Repository

```
supersapi        launcher (bash)
install.sh       installer
sapid/           broker (Rust): policy, runtime, swarm, server, protocol, client
extension/       pi extension (TypeScript): swarm tools and /swarm
image/           Dockerfile (sapi's image + the extension)
docs/            the five documents above
test/            check.sh (no Docker), smoke.sh (Docker), extension harness
scripts/         release packaging
```

Contributions that add dependencies, widen what a child can reach, or move Docker access into a container need a very good reason. Run `test/check.sh` before opening a PR; CI runs `smoke.sh` with real Docker.

## License

[MIT](LICENSE)
