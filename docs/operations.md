# Operations and debugging

## Where things are

```
~/.local/bin/supersapi                 launcher
~/.local/bin/sapid                     broker
~/.local/share/supersapi/image/        Dockerfile
~/.local/share/supersapi/extension/    pi extension
~/.local/state/supersapi/<swarm>/      per-directory swarm state
    sapid.sock      broker socket (unix transport)
    token           root token, 0600, removed on exit
    sapid.json      {pid, addr, addr_in_container, root_dir, image, version}, removed on exit
    sapid.log       broker log (append-only across runs)
    nodes/NNN.jsonl complete pi JSON stream of each child, plus {"type":"stderr",…} lines
```

`<swarm>` = first 8 hex of `sha256(pwd -P)`. `supersapi ps` in the directory shows the tree; `supersapi logs 003` prints a child's whole stream.

## First run

```sh
supersapi doctor
```

It reports the Docker flavour, the transport it will use, builds the image, confirms the extension is in it, then starts a throwaway broker and makes a container connect to it and say `hello`. If that last check fails, the socket cannot be mounted on your Docker; use `SUPERSAPI_TRANSPORT=tcp`.

## Reading the broker log

`sapid` logs to stderr with `tracing`; the launcher redirects it to `sapid.log`. Levels via `SAPID_LOG` (`info` default; `debug` shows every supervision step; `sapid=trace` for everything). `SAPID_LOG_FORMAT=json` for machine-readable lines.

Key messages:

| message | meaning |
|---|---|
| `serving` | bound; prints swarm id, address, root dir, image |
| `spawn node=003 parent=001 dir=… access=…` | a child was created |
| `finished node=003 state=Exited code=Some(0)` | child exited |
| `timeout after Ns; stopping` | child hit its budget; `docker stop` issued |
| `output pipes still open after stop; abandoning them` | a process held stdout past the grace period; supervision ended anyway |
| `watched process is gone` | the launcher died; shutdown follows |
| `shutting down reason=…` | SIGINT/SIGTERM, root `shutdown`, or launcher exit |

## Talking to the broker by hand

```sh
cd the/project
S=~/.local/state/supersapi/$(printf '%s' "$(pwd -P)" | sha256sum | cut -c1-8)
sapid ctl --addr "$(jq -r .addr $S/sapid.json)" --token-file $S/token hello
sapid ctl … tree
sapid ctl … spawn --dir src --access read-only --wait "Summarise the architecture in 10 lines."
sapid ctl … logs 001
sapid ctl … stop 001
```

`SAPID_ADDR` and `SAPID_TOKEN` are also honoured, which is exactly what a container has — so from inside any node, `sapid ctl hello` works if you copy the binary in. The extension does the same thing in TypeScript.

## Plain Docker

Children are ordinary containers:

```sh
docker ps --filter label=sh.supersapi.managed=true \
  --format 'table {{.Names}}\t{{.Status}}\t{{.Label "sh.supersapi.node"}}\t{{.Label "sh.supersapi.parent"}}'
docker inspect ssapi-<swarm>-003     # the complete spec sapid produced
docker exec -it ssapi-<swarm>-003 bash
```

`--rm` means a finished child leaves nothing behind except its log in the state dir.

## Headless and in scripts

```sh
supersapi run "Update CHANGELOG.md from the git log since the last tag." > out.md
echo $?      # 0 if the child exited 0
```

`run` starts a broker, spawns one child of the root with the whole directory, prints its final assistant message on stdout and shuts the broker down. Progress goes to stderr. Compose it with anything: cron, CI, `xargs -P`, a Makefile.

## Common problems

**`supersapi: a swarm is already running for …`** — another launcher owns this directory. `supersapi ps` shows it. If it is stale (the pid in `sapid.json` is dead), the message is wrong only if `sapid.json` outlived the process — remove the state dir for that swarm.

**`doctor` fails the connectivity check on macOS** — Docker Desktop: set `SUPERSAPI_TRANSPORT=tcp`. OrbStack and colima support Unix socket mounts.

**Children exit immediately with a provider error** — they share the root's credentials volume; log in once in the root (`/login`) or pass a key with `SAPI_ENV`. Children themselves receive no host environment.

**`policy_violation: directory escapes the caller's workspace`** — the agent asked for `..`, an absolute path, or a symlink pointing outside. Working as intended.

**`limit_exceeded`** — raise `SUPERSAPI_MAX_CHILDREN` / `MAX_TOTAL` / `MAX_DEPTH` on the launcher, or have the agent stop finished children. Finished children do not count.

**Image build fails at `COPY <<EOF`** — Docker older than 23 without BuildKit. Upgrade Docker or install `docker-buildx`.

**Root-owned files on Linux** — only if you forced `SAPI_UID=0`; by default children run as your uid/gid.

## Upgrading

`supersapi upgrade` re-runs the installer. The image digest includes the Dockerfile and extension sources, so a new release yields a new image on next start. `SUPERSAPI_REBUILD=1 supersapi build` forces `--no-cache --pull`.

## Removing everything

```sh
docker ps -aq --filter label=sh.supersapi.managed=true | xargs -r docker rm -f
docker volume ls -q --filter name=supersapi- | xargs -r docker volume rm
rm -rf ~/.local/state/supersapi ~/.local/share/supersapi ~/.local/bin/supersapi ~/.local/bin/sapid
```

Credentials live in `sapi-config`, shared with sapi; remove with `docker volume rm sapi-config` if you mean it.
