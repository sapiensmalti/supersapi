# Security

## The boundary

supersapi exists so that an agent can create agents **without anything in a container ever holding Docker, host, or sibling access**. The boundary is the broker. Everything on the container side of it is assumed to be driven by a model that may be mistaken, manipulated by file contents it reads, or simply wrong.

## Why the Docker socket is not an option

Mounting `/var/run/docker.sock` into a container grants the Docker API, and the Docker API grants root on the host: `docker run -v /:/host --privileged` from inside is a complete escape. Proxies that filter the API (`docker-socket-proxy` and friends) still expose `create` with arbitrary mounts, because that is what spawning *is*. The only safe shape is a broker that does not accept container specifications at all — it accepts a *task and a narrowing*, and builds the specification itself. That is `sapid`.

## What a node can do

With its token, through nine verbs: learn its own profile, spawn a child **smaller than itself**, and observe or stop **its own descendants**. That is the complete list. See [protocol.md](protocol.md) and [isolation.md](isolation.md).

## What a node cannot do

- Reach Docker, the host filesystem outside its directory, the host network namespace, or other containers.
- Learn host paths (`NodeInfo.dir` is relative), the broker's state directory, or any token but its own.
- See, wait on, read logs of, or stop its parent, siblings, or the root.
- Widen anything: directory, access, network, time, cpu, memory, depth.
- Forge a request: tokens are 256-bit random, compared by SHA-256 lookup, carried in the request body.
- Exhaust the host: `max_children`, `max_total`, `--pids-limit`, `--cpus`, `--memory` and per-child timeouts are enforced by the broker and the kernel, not by the agent's good manners.
- Make the broker trust a `.pi/` directory it finds: children run `--no-approve` unless the spawner opts in with `approve_project`.

## Host-side surface

- `sapid` listens on a Unix socket with mode `0600` in a `0700` state directory, or on TCP bound to loopback (macOS) / the docker bridge gateway (Linux). TCP is reachable by any process on that interface; the token is what matters.
- `sapid` runs as you, never as root. It runs `docker`; whatever `docker` can do, `sapid` could do — which is exactly why its spawn path builds the argv itself from a `Profile` and the agent never supplies Docker arguments. `--docker-arg` on the command line is an operator knob, not a protocol field.
- State files (`token`, `sapid.json`, logs) are created `0600`. Logs contain everything the child printed, including anything it read from your files. Treat `~/.local/state/supersapi` like a shell history.

## Supply chain

- Release archives are built by GitHub Actions from a tag, with checksums in `SHA256SUMS`; the installer verifies them. Pin a version with `SUPERSAPI_VERSION=vX.Y.Z`.
- The broker has a short dependency list (tokio, serde, clap, tracing, rand, sha2, subtle, thiserror, anyhow, libc) and a committed `Cargo.lock`. CI runs `cargo deny` for advisories and license policy.
- The image installs `pi` from npm and base layers from Docker Hub/GHCR at build time. Children run with `SAPI_AUTO_UPDATE=0`; the root self-updates on start unless `SAPI_AUTO_UPDATE=0`.

## What this does not protect against

- **Kernel escapes.** Containers share a kernel. Use VM-backed Docker for untrusted code.
- **Damage inside a `read_write` directory.** A child can delete everything it can see. Give implementers the narrowest directory that works, give everyone else `read_only`, and commit first.
- **Exfiltration over the network** by a child with `network: full` — the same as any agent with internet. `network: none` exists, but then the child cannot reach a model provider either unless the model is local.
- **Cost.** The broker reports usage; it does not cap dollars.

## Reporting

Open an issue. Mark it sensitive in the title and we will move to a private channel.
