//! `sapid` — the supersapi swarm broker.
//!
//! Runs on the host. Agent containers talk to it over a socket and may ask it
//! to spawn, observe and stop *their own* children. Every child gets a subset
//! of its parent's capabilities and nothing more. The Docker socket never
//! enters a container.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod client;
mod policy;
mod protocol;
mod runtime;
mod server;
mod swarm;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use client::{Addr, Client};
use policy::Profile;
use protocol::{Access, Frame, Limits, LogsParams, Network, NodeRef, Op, SpawnParams, WaitParams};
use runtime::{Docker, Transport};
use server::Listener;
use swarm::{Swarm, SwarmConfig};

/// supersapi swarm broker.
#[derive(Parser, Debug)]
#[command(name = "sapid", version, about, long_about = None)]
struct Cli {
    /// Log format.
    #[arg(long, global = true, value_enum, default_value_t = LogFormat::Text, env = "SAPID_LOG_FORMAT")]
    log_format: LogFormat,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the broker for one swarm.
    Serve(ServeArgs),
    /// Talk to a running broker.
    Ctl(CtlArgs),
}

#[derive(clap::Args, Debug)]
struct ServeArgs {
    /// Directory for the socket, token, and per-node logs.
    #[arg(long)]
    state_dir: PathBuf,
    /// Root workspace on the host.
    #[arg(long)]
    root_dir: PathBuf,
    /// Image to run children from.
    #[arg(long)]
    image: String,
    /// Docker volume holding pi credentials and settings.
    #[arg(long, default_value = "sapi-config")]
    config_volume: String,
    /// `unix` (socket in state dir) or `tcp:HOST:PORT` (`PORT` may be 0).
    #[arg(long, default_value = "unix")]
    listen: String,
    /// Add `host.docker.internal:host-gateway` to children (TCP on Linux).
    #[arg(long)]
    add_host_gateway: bool,
    /// uid children run as (Linux). Omit for root.
    #[arg(long)]
    uid: Option<u32>,
    /// gid children run as (Linux).
    #[arg(long)]
    gid: Option<u32>,
    /// Filesystem access of the root (and ceiling for everyone).
    #[arg(long, value_enum, default_value_t = AccessArg::ReadWrite)]
    access: AccessArg,
    /// Network reach of the root (and ceiling for everyone).
    #[arg(long, value_enum, default_value_t = NetworkArg::Full)]
    network: NetworkArg,
    /// Per-child wall-clock budget ceiling, seconds.
    #[arg(long, default_value_t = 3600)]
    timeout_secs: u64,
    /// Per-child CPU ceiling.
    #[arg(long, default_value_t = 2.0)]
    cpus: f64,
    /// Per-child memory ceiling, MiB.
    #[arg(long, default_value_t = 4096)]
    memory_mib: u64,
    /// Running children per node.
    #[arg(long, default_value_t = 4)]
    max_children: u32,
    /// Tree depth below the root.
    #[arg(long, default_value_t = 2)]
    max_depth: u32,
    /// Running nodes in the whole swarm.
    #[arg(long, default_value_t = 16)]
    max_total: u32,
    /// Exit (stopping every child) when this process disappears.
    #[arg(long)]
    exit_with_pid: Option<i32>,
    /// `docker` binary.
    #[arg(long, default_value = "docker", env = "SAPID_DOCKER")]
    docker: PathBuf,
    /// Extra `docker run` argument for every child (repeatable).
    #[arg(long = "docker-arg")]
    docker_args: Vec<String>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum AccessArg {
    None,
    ReadOnly,
    ReadWrite,
}

impl From<AccessArg> for Access {
    fn from(a: AccessArg) -> Self {
        match a {
            AccessArg::None => Access::None,
            AccessArg::ReadOnly => Access::ReadOnly,
            AccessArg::ReadWrite => Access::ReadWrite,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum NetworkArg {
    None,
    Full,
}

impl From<NetworkArg> for Network {
    fn from(n: NetworkArg) -> Self {
        match n {
            NetworkArg::None => Network::None,
            NetworkArg::Full => Network::Full,
        }
    }
}

#[derive(clap::Args, Debug)]
struct CtlArgs {
    /// Broker address (`unix://PATH` or `tcp://HOST:PORT`).
    #[arg(long, env = "SAPID_ADDR")]
    addr: String,
    /// Token.
    #[arg(long, env = "SAPID_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Read the token from a file.
    #[arg(long, conflicts_with = "token")]
    token_file: Option<PathBuf>,
    #[command(subcommand)]
    op: CtlOp,
}

#[derive(Subcommand, Debug)]
enum CtlOp {
    /// Who am I, and what are the limits.
    Hello,
    /// Spawn a child.
    Spawn {
        /// Task prompt.
        task: String,
        /// Directory relative to the caller's workspace.
        #[arg(long)]
        dir: Option<String>,
        #[arg(long, value_enum)]
        access: Option<AccessArg>,
        #[arg(long, value_enum)]
        network: Option<NetworkArg>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        timeout_secs: Option<u64>,
        #[arg(long)]
        name: Option<String>,
        /// Wait for it and print the result.
        #[arg(long)]
        wait: bool,
    },
    /// List descendants.
    List,
    /// Pretty-print the tree.
    Tree,
    /// Describe one node.
    Status { id: String },
    /// Wait for a node.
    Wait {
        id: String,
        /// Print progress events.
        #[arg(long)]
        stream: bool,
        #[arg(long)]
        timeout_secs: Option<u64>,
    },
    /// Print a node's log.
    Logs {
        id: String,
        #[arg(long)]
        follow: bool,
    },
    /// Stop a node and its subtree.
    Stop { id: String },
    /// Stop everything and exit the broker (root only).
    Shutdown,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.log_format);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("tokio runtime")?;
    match cli.cmd {
        Cmd::Serve(a) => rt.block_on(serve(a)),
        Cmd::Ctl(a) => rt.block_on(ctl(a)),
    }
}

fn init_logging(format: LogFormat) {
    let filter = EnvFilter::try_from_env("SAPID_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    match format {
        LogFormat::Text => builder.with_target(false).init(),
        LogFormat::Json => builder.json().init(),
    }
}

async fn serve(a: ServeArgs) -> Result<()> {
    let root_dir = a
        .root_dir
        .canonicalize()
        .with_context(|| format!("root dir {}", a.root_dir.display()))?;
    if !root_dir.is_dir() {
        bail!("root dir is not a directory: {}", root_dir.display());
    }
    std::fs::create_dir_all(a.state_dir.join("nodes")).context("create state dir")?;
    let state_dir = a.state_dir.canonicalize().context("state dir")?;
    restrict_dir(&state_dir)?;

    let docker = Docker::new(&a.docker);
    if !docker.is_available().await {
        bail!("docker is not available ({})", a.docker.display());
    }

    let (listener, transport, addr) = bind(&a, &state_dir).await?;

    let swarm_id = state_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("swarm")
        .to_owned();
    let user = match (a.uid, a.gid) {
        (Some(u), Some(g)) => Some((u, g)),
        (Some(u), None) => Some((u, u)),
        (None, _) => None,
    };
    let cfg = SwarmConfig {
        id: swarm_id.clone(),
        state_dir: state_dir.clone(),
        image: a.image.clone(),
        config_volume: a.config_volume.clone(),
        transport,
        user,
        limits: Limits {
            max_children: a.max_children,
            max_depth: a.max_depth,
            max_total: a.max_total,
        },
        extra_docker_args: a.docker_args.clone(),
    };
    let root = Profile {
        host_dir: root_dir.clone(),
        access: a.access.into(),
        network: a.network.into(),
        timeout_secs: a.timeout_secs,
        cpus: a.cpus,
        memory_mib: a.memory_mib,
        depth: 0,
    };
    let (swarm, root_token) = Swarm::new(cfg, docker, root);

    write_private(&state_dir.join("token"), root_token.as_bytes())?;
    write_private(
        &state_dir.join("sapid.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "pid": std::process::id(),
            "addr": addr.to_url(),
            "addr_in_container": swarm.config().transport.address_for_container(),
            "root_dir": root_dir,
            "image": a.image,
            "version": env!("CARGO_PKG_VERSION"),
        }))?
        .as_slice(),
    )?;

    info!(swarm = %swarm_id, addr = %addr.to_url(), root = %root_dir.display(), image = %a.image, "serving");

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(server::serve(listener, swarm.clone(), shutdown_rx));

    let reason = tokio::select! {
        r = wait_for_signal() => r,
        () = watch_pid(a.exit_with_pid) => "launcher exited".to_owned(),
        () = wait_for_shutdown_op(&swarm) => "shutdown requested".to_owned(),
    };

    info!(%reason, "shutting down");
    let stopped = swarm.shutdown(&reason).await;
    if !stopped.is_empty() {
        info!(count = stopped.len(), "children stopped");
    }
    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
    if let Transport::Unix(p) = &swarm.config().transport {
        let _ = std::fs::remove_file(p);
    }
    let _ = std::fs::remove_file(state_dir.join("token"));
    let _ = std::fs::remove_file(state_dir.join("sapid.json"));
    Ok(())
}

async fn bind(a: &ServeArgs, state_dir: &Path) -> Result<(Listener, Transport, Addr)> {
    if a.listen == "unix" {
        let sock = state_dir.join("sapid.sock");
        let l = Listener::bind_unix(&sock).with_context(|| format!("bind {}", sock.display()))?;
        return Ok((l, Transport::Unix(sock.clone()), Addr::Unix(sock)));
    }
    if let Some(addr) = a.listen.strip_prefix("tcp:") {
        let l = Listener::bind_tcp(addr)
            .await
            .with_context(|| format!("bind {addr}"))?;
        let port = l.tcp_port().ok_or_else(|| anyhow!("no tcp port"))?;
        let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h);
        return Ok((
            l,
            Transport::Tcp {
                port,
                add_host: a.add_host_gateway,
            },
            Addr::Tcp(format!("{host}:{port}")),
        ));
    }
    bail!("--listen must be `unix` or `tcp:HOST:PORT`")
}

fn restrict_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(unix)]
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("write {}", path.display()))?;
    f.write_all(data)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    std::fs::write(path, data).with_context(|| format!("write {}", path.display()))
}

async fn wait_for_signal() -> String {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            error!("cannot install SIGTERM handler: {e}");
            std::future::pending::<()>().await;
            return String::new();
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => "SIGINT".to_owned(),
        _ = term.recv() => "SIGTERM".to_owned(),
    }
}

/// Resolve when `pid` is gone. Never resolves when `pid` is `None`.
async fn watch_pid(pid: Option<i32>) {
    let Some(pid) = pid else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if !pid_alive(pid) {
            warn!(pid, "watched process is gone");
            return;
        }
    }
}

fn pid_alive(pid: i32) -> bool {
    // `kill -0` without unsafe: ask the OS through /proc on Linux, `ps` elsewhere.
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "pid="])
            .output()
            .map(|o| o.status.success() && !o.stdout.is_empty())
            .unwrap_or(false)
    }
}

async fn wait_for_shutdown_op(swarm: &Swarm) {
    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if swarm.is_shutting_down() {
            return;
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn ctl(a: CtlArgs) -> Result<()> {
    let token = match (&a.token, &a.token_file) {
        (Some(t), _) => t.clone(),
        (None, Some(f)) => client::read_token(f)?,
        (None, None) => bail!("provide --token, SAPID_TOKEN, or --token-file"),
    };
    let addr = Addr::parse(&a.addr)?;
    let mut c = Client::connect(&addr, token).await?;
    let print_event = |e: &serde_json::Value| {
        if let Some(line) = e.get("line").and_then(serde_json::Value::as_str) {
            println!("{line}");
        } else {
            println!("{e}");
        }
    };

    let frame = match a.op {
        CtlOp::Hello => c.call(Op::Hello, print_event).await?,
        CtlOp::Spawn {
            task,
            dir,
            access,
            network,
            model,
            timeout_secs,
            name,
            wait,
        } => {
            let params = SpawnParams {
                task,
                dir,
                access: access.map(Into::into),
                network: network.map(Into::into),
                model,
                timeout_secs,
                cpus: None,
                memory_mib: None,
                name,
                approve_project: false,
            };
            let f = c.call(Op::Spawn(params), print_event).await?;
            match (&f, wait) {
                (Frame::Ok { result, .. }, true) => {
                    let id = result
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| anyhow!("no id in spawn result"))?
                        .to_owned();
                    eprintln!("spawned {id}; waiting");
                    c.call(
                        Op::Wait(WaitParams {
                            id,
                            stream: true,
                            timeout_secs: None,
                        }),
                        print_event,
                    )
                    .await?
                }
                _ => f,
            }
        }
        CtlOp::List => c.call(Op::List, print_event).await?,
        CtlOp::Tree => {
            let f = c.call(Op::List, print_event).await?;
            if let Frame::Ok { result, .. } = &f {
                print_tree(result);
                return Ok(());
            }
            f
        }
        CtlOp::Status { id } => c.call(Op::Status(NodeRef { id }), print_event).await?,
        CtlOp::Wait {
            id,
            stream,
            timeout_secs,
        } => {
            c.call(
                Op::Wait(WaitParams {
                    id,
                    stream,
                    timeout_secs,
                }),
                print_event,
            )
            .await?
        }
        CtlOp::Logs { id, follow } => {
            c.call(Op::Logs(LogsParams { id, follow }), print_event)
                .await?
        }
        CtlOp::Stop { id } => c.call(Op::Stop(NodeRef { id }), print_event).await?,
        CtlOp::Shutdown => c.call(Op::Shutdown, print_event).await?,
    };

    match frame {
        Frame::Ok { result, .. } => {
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        Frame::Err { error, .. } => {
            eprintln!("error [{:?}]: {}", error.code, error.message);
            std::process::exit(1)
        }
        Frame::Event { .. } => Ok(()),
    }
}

fn print_tree(result: &serde_json::Value) {
    let Some(nodes) = result.get("nodes").and_then(serde_json::Value::as_array) else {
        return;
    };
    if nodes.is_empty() {
        println!("(no children)");
        return;
    }
    println!(
        "{:<5} {:<7} {:<8} {:<10} {:<6} {:<20} TASK",
        "ID", "DEPTH", "STATE", "ACCESS", "TURNS", "DIR"
    );
    for n in nodes {
        let g = |k: &str| {
            n.get(k)
                .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
                .unwrap_or_default()
        };
        let depth = n
            .get("depth")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1)
            .saturating_sub(1);
        let indent = "  ".repeat(usize::try_from(depth).unwrap_or(0));
        let turns = n
            .get("usage")
            .and_then(|u| u.get("turns"))
            .map(std::string::ToString::to_string)
            .unwrap_or_default();
        println!(
            "{:<5} {:<7} {:<8} {:<10} {:<6} {:<20} {}{}",
            g("id"),
            g("depth"),
            g("state"),
            g("access"),
            turns,
            g("dir"),
            indent,
            g("task")
        );
    }
}
