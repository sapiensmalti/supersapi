# Architecture

## Pieces

| Piece | Language | Runs on | Role |
|---|---|---|---|
| `supersapi` | bash | host | Launcher. Builds the image, starts `sapid`, runs the root container, cleans up. |
| `sapid` | Rust | host | Broker. Owns the node tree and tokens, enforces policy, runs and supervises child containers, serves the protocol. |
| `extension/` | TypeScript | inside every container | pi extension: `swarm`, `swarm_ps`, `swarm_stop`, `/swarm`. Pure protocol client. |
| `image/Dockerfile` | — | — | sapi's image plus `/opt/supersapi/extension`. |

```
supersapi
   ├── docker build  (once per image digest)
   ├── sapid serve --exit-with-pid $$        ← background, per directory
   │      writes  ~/.local/state/supersapi/<swarm>/{sapid.sock,token,sapid.json,sapid.log,nodes/*.jsonl}
   └── docker run -it … root container       ← foreground; token via env, socket bind-mounted
          └── pi --extension /opt/supersapi/extension/index.ts --continue
                 └── swarm tool ──socket──▶ sapid ──▶ docker run … child
                                                          └── pi --mode json -p --no-session -- "task"
```

`<swarm>` is the first 8 hex digits of `sha256(absolute working directory)`, so `supersapi ps` from any shell in that directory finds the right broker.

## sapid internals

```
main.rs      CLI (serve, ctl), startup/shutdown, signal and parent-pid watch
server.rs    listener (unix|tcp), per-connection reader, per-request tasks, serialized writer
swarm.rs     node registry, token store, spawn, supervision loop, log capture, stop/shutdown
policy.rs    Profile, derive_child(): the attenuation rules. Pure.
runtime.rs   ChildSpec::argv(): the docker run command. Docker::{run,stop,is_available}.
protocol.rs  wire types (serde), error codes, frame constructors
client.rs    minimal client for `sapid ctl` and tests
```

**Concurrency.** Tokio multi-thread runtime. The swarm state is one `std::sync::Mutex<Inner>` held only for short, non-awaiting critical sections. Each child has a `watch` channel for state (`NodeInfo`) and a `broadcast` channel for log lines; `wait` and `logs` subscribe to those, so the supervisor never knows who is listening.

**Supervision.** `docker run --rm` is spawned with piped stdout/stderr. Two tasks read the pipes line by line into an `mpsc`; the supervisor loop appends to `nodes/<id>.jsonl`, folds `message_end` assistant messages into usage/output, and publishes the line. The pipes closing is the completion signal; after a timeout `docker stop` is issued and a hard deadline guarantees the loop ends even if something keeps stdout open. `finish()` is idempotent, so a `stop` racing an exit records one terminal state.

**Tokens.** 32 random bytes, hex. Only the SHA-256 is stored. The token reaches the container through the environment of the `docker` process (`-e SAPID_TOKEN` with no value) so it is never in `ps` output or shell history. The root's token is written to `state/token` (mode 0600) for the launcher and `sapid ctl`.

**Lifetime.** `sapid` exits on SIGINT/SIGTERM, when the root sends `shutdown`, or when the pid passed with `--exit-with-pid` (the launcher) disappears. In every case it stops all running children first, then removes its socket, token and `sapid.json`.

## Transport

Unix socket by default: owner-only file permissions, no port, nothing on the network. Docker Desktop's VM cannot bind-mount host Unix sockets, so the launcher detects it and switches to TCP on `127.0.0.1:0`, reachable from containers as `host.docker.internal`. On Linux with `tcp`, the broker binds the docker bridge gateway address and adds `host-gateway` to children. Either way every request carries a token; the transport is not the security boundary.

## Why Rust for the broker

It is the one component that is long-running, handles untrusted input (anything a model-driven container sends), and decides what Docker does. That calls for a language with a real type system, no GC pauses, static binaries, and a lint culture that refuses `unwrap` in production code. The crate builds with `#![forbid(unsafe_code)]`, `clippy::pedantic`, `unwrap_used`/`expect_used`/`panic` denied, and a single `Cargo.lock`.

## Why bash for the launcher and TypeScript for the extension

The launcher is glue around `docker` and `sapid`; it must be readable by anyone in one sitting and need no build step. The extension must be TypeScript because that is what pi loads — and it is kept to erasable syntax so it also runs under plain Node for testing.

## Testing

| Layer | How | Where |
|---|---|---|
| policy, argv, protocol | unit tests | `cargo test` |
| broker end to end | `sapid serve` + `sapid ctl` with `tests/fake-docker.sh` as `docker`; the fake child even spawns grandchildren through the real socket | `cargo test --test e2e` |
| extension | real extension, fake `ExtensionAPI`, real broker, fake docker | `test/extension-harness.mjs` |
| launcher | `supersapi run` with a docker shim | `test/check.sh` |
| the real thing | image build, `doctor`, `run`, `ps`, `stop` with Docker | `test/smoke.sh` (CI, Ubuntu) |
