//! The swarm: node registry, token authority and child supervision.
//!
//! One [`Swarm`] exists per broker. It owns every node, hands out tokens,
//! runs each child's container to completion, records its output and keeps a
//! JSONL log per node on disk. All policy decisions are delegated to
//! [`crate::policy`]; all Docker knowledge lives in [`crate::runtime`].

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::RngCore;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, watch};
use tracing::{debug, info, warn};

use crate::policy::{self, Population, Profile};
use crate::protocol::{Access, ErrorCode, Limits, NodeInfo, SpawnParams, State, Usage};
use crate::runtime::{ChildSpec, Docker, Transport};

/// A node id. Zero-padded decimal, root is `000`.
pub type NodeId = String;

const TOKEN_BYTES: usize = 32;
const LOG_CHANNEL_CAPACITY: usize = 4096;
const STOP_GRACE_SECS: u32 = 10;
const TASK_PREVIEW_CHARS: usize = 120;

/// Error returned by swarm operations, already mapped to a protocol code.
#[derive(Debug, Clone)]
pub struct SwarmError {
    /// Protocol error code.
    pub code: ErrorCode,
    /// Explanation.
    pub message: String,
}

impl SwarmError {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<policy::PolicyError> for SwarmError {
    fn from(e: policy::PolicyError) -> Self {
        use policy::PolicyError as P;
        let code = match e {
            P::TooDeep(_) | P::TooManyChildren(_) | P::TooManyTotal(_) => ErrorCode::LimitExceeded,
            _ => ErrorCode::PolicyViolation,
        };
        SwarmError::new(code, e.to_string())
    }
}

/// One raw line of a child's output, numbered from zero.
#[derive(Debug, Clone)]
pub struct LogLine {
    /// Sequence number within the node's log.
    pub seq: u64,
    /// The line (JSON for stdout events, a `stderr` object for stderr).
    pub line: Arc<str>,
}

struct Node {
    info: NodeInfo,
    profile: Profile,
    container: String,
    log_path: PathBuf,
    log_lines: u64,
    output: String,
    state_tx: watch::Sender<NodeInfo>,
    log_tx: broadcast::Sender<LogLine>,
}

struct Inner {
    nodes: HashMap<NodeId, Node>,
    tokens: HashMap<[u8; 32], NodeId>,
    next_seq: u32,
    shutting_down: bool,
}

/// Static configuration of a swarm.
#[derive(Debug, Clone)]
pub struct SwarmConfig {
    /// Short random id, used in container names and labels.
    pub id: String,
    /// Where logs are written: `<state_dir>/nodes/<id>.jsonl`.
    pub state_dir: PathBuf,
    /// Image to run children from.
    pub image: String,
    /// Docker volume with pi credentials and settings.
    pub config_volume: String,
    /// How children reach the broker.
    pub transport: Transport,
    /// uid:gid children run as; `None` for root.
    pub user: Option<(u32, u32)>,
    /// Fan-out and depth limits.
    pub limits: Limits,
    /// Extra `docker run` arguments for every child.
    pub extra_docker_args: Vec<String>,
}

/// The swarm.
#[derive(Clone)]
pub struct Swarm {
    cfg: Arc<SwarmConfig>,
    docker: Docker,
    inner: Arc<Mutex<Inner>>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn hash_token(token: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    h.finalize().into()
}

fn new_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes
        .iter()
        .fold(String::with_capacity(TOKEN_BYTES * 2), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn preview(task: &str) -> String {
    let first = task.lines().next().unwrap_or("");
    let mut s: String = first.chars().take(TASK_PREVIEW_CHARS).collect();
    if first.chars().count() > TASK_PREVIEW_CHARS || task.lines().count() > 1 {
        s.push('…');
    }
    s
}

impl Swarm {
    /// Create a swarm with its root node. Returns the swarm and the root token.
    pub fn new(cfg: SwarmConfig, docker: Docker, root: Profile) -> (Self, String) {
        let token = new_token();
        let info = NodeInfo {
            id: "000".to_owned(),
            parent: None,
            depth: 0,
            name: "root".to_owned(),
            task: "interactive".to_owned(),
            dir: ".".to_owned(),
            access: root.access,
            network: root.network,
            timeout_secs: root.timeout_secs,
            state: State::Running,
            exit_code: None,
            reason: None,
            started_at: now(),
            finished_at: None,
            usage: Usage::default(),
            model: None,
        };
        let (state_tx, _) = watch::channel(info.clone());
        let (log_tx, _) = broadcast::channel(LOG_CHANNEL_CAPACITY);
        let node = Node {
            info,
            profile: root,
            container: String::new(),
            log_path: cfg.state_dir.join("nodes").join("000.jsonl"),
            log_lines: 0,
            output: String::new(),
            state_tx,
            log_tx,
        };
        let mut tokens = HashMap::new();
        tokens.insert(hash_token(&token), "000".to_owned());
        let mut nodes = HashMap::new();
        nodes.insert("000".to_owned(), node);
        let swarm = Self {
            cfg: Arc::new(cfg),
            docker,
            inner: Arc::new(Mutex::new(Inner {
                nodes,
                tokens,
                next_seq: 1,
                shutting_down: false,
            })),
        };
        (swarm, token)
    }

    /// Configuration.
    pub fn config(&self) -> &SwarmConfig {
        &self.cfg
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A poisoned lock means a panic while holding it; the data is still
        // consistent enough to continue serving, and refusing would strand
        // every running child.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Resolve a token to a node id.
    pub fn authenticate(&self, token: &str) -> Result<NodeId, SwarmError> {
        let h = hash_token(token);
        self.lock()
            .tokens
            .get(&h)
            .cloned()
            .ok_or_else(|| SwarmError::new(ErrorCode::Unauthorized, "unknown token"))
    }

    /// Public info for a node.
    pub fn info(&self, id: &str) -> Option<NodeInfo> {
        self.lock().nodes.get(id).map(|n| n.info.clone())
    }

    /// Limits.
    pub fn limits(&self) -> Limits {
        self.cfg.limits
    }

    fn is_descendant(inner: &Inner, ancestor: &str, id: &str) -> bool {
        let mut cur = Some(id.to_owned());
        while let Some(c) = cur {
            if c == ancestor {
                return true;
            }
            cur = inner.nodes.get(&c).and_then(|n| n.info.parent.clone());
        }
        false
    }

    /// Check that `id` exists and is a strict descendant of `caller`.
    pub fn authorize(&self, caller: &str, id: &str) -> Result<(), SwarmError> {
        let inner = self.lock();
        if !inner.nodes.contains_key(id) {
            return Err(SwarmError::new(
                ErrorCode::NotFound,
                format!("no node {id}"),
            ));
        }
        if id == caller || !Self::is_descendant(&inner, caller, id) {
            return Err(SwarmError::new(
                ErrorCode::Forbidden,
                format!("{id} is not a descendant of {caller}"),
            ));
        }
        Ok(())
    }

    /// Descendants of `caller`, depth-first in spawn order.
    pub fn list(&self, caller: &str) -> Vec<NodeInfo> {
        let inner = self.lock();
        let mut ids: Vec<&NodeId> = inner
            .nodes
            .keys()
            .filter(|id| id.as_str() != caller && Self::is_descendant(&inner, caller, id))
            .collect();
        ids.sort();
        ids.iter()
            .filter_map(|id| inner.nodes.get(*id))
            .map(|n| n.info.clone())
            .collect()
    }

    /// Subscribe to a node's state and log stream.
    pub fn subscribe(
        &self,
        id: &str,
    ) -> Option<(
        watch::Receiver<NodeInfo>,
        broadcast::Receiver<LogLine>,
        u64,
        PathBuf,
    )> {
        let inner = self.lock();
        let n = inner.nodes.get(id)?;
        Some((
            n.state_tx.subscribe(),
            n.log_tx.subscribe(),
            n.log_lines,
            n.log_path.clone(),
        ))
    }

    /// Final output of a node (last assistant message).
    pub fn output(&self, id: &str) -> String {
        self.lock()
            .nodes
            .get(id)
            .map(|n| n.output.clone())
            .unwrap_or_default()
    }

    /// Spawn a child of `caller`.
    #[allow(clippy::too_many_lines)]
    pub fn spawn(
        &self,
        caller: &str,
        params: &SpawnParams,
    ) -> Result<(NodeId, String), SwarmError> {
        if params.task.trim().is_empty() {
            return Err(SwarmError::new(
                ErrorCode::BadRequest,
                "task must not be empty",
            ));
        }

        let (id, token, spec, info) = {
            let mut inner = self.lock();
            if inner.shutting_down {
                return Err(SwarmError::new(
                    ErrorCode::ShuttingDown,
                    "broker is shutting down",
                ));
            }
            let parent = inner
                .nodes
                .get(caller)
                .ok_or_else(|| SwarmError::new(ErrorCode::Unauthorized, "caller vanished"))?;
            let population = Population {
                running_children: inner
                    .nodes
                    .values()
                    .filter(|n| {
                        n.info.parent.as_deref() == Some(caller) && n.info.state == State::Running
                    })
                    .count()
                    .try_into()
                    .unwrap_or(u32::MAX),
                running_total: inner
                    .nodes
                    .values()
                    .filter(|n| n.info.state == State::Running)
                    .count()
                    .try_into()
                    .unwrap_or(u32::MAX),
            };
            let profile =
                policy::derive_child(&parent.profile, params, self.cfg.limits, population)?;

            let root_dir = inner
                .nodes
                .get("000")
                .map(|n| n.profile.host_dir.clone())
                .unwrap_or_default();
            let id = format!("{:03}", inner.next_seq);
            inner.next_seq += 1;
            let token = new_token();
            let container = format!("ssapi-{}-{id}", self.cfg.id);

            let info = NodeInfo {
                id: id.clone(),
                parent: Some(caller.to_owned()),
                depth: profile.depth,
                name: params.name.clone().unwrap_or_else(|| id.clone()),
                task: preview(&params.task),
                dir: if profile.access == Access::None {
                    "(scratch)".to_owned()
                } else {
                    policy::display_relative(&root_dir, &profile.host_dir)
                },
                access: profile.access,
                network: profile.network,
                timeout_secs: profile.timeout_secs,
                state: State::Running,
                exit_code: None,
                reason: None,
                started_at: now(),
                finished_at: None,
                usage: Usage::default(),
                model: params.model.clone(),
            };
            let spec = ChildSpec {
                name: container.clone(),
                image: self.cfg.image.clone(),
                swarm: self.cfg.id.clone(),
                node: id.clone(),
                parent: caller.to_owned(),
                profile: profile.clone(),
                config_volume: self.cfg.config_volume.clone(),
                transport: self.cfg.transport.clone(),
                user: self.cfg.user,
                model: params.model.clone(),
                approve_project: params.approve_project,
                task: params.task.clone(),
                extra_args: self.cfg.extra_docker_args.clone(),
            };
            let (state_tx, _) = watch::channel(info.clone());
            let (log_tx, _) = broadcast::channel(LOG_CHANNEL_CAPACITY);
            inner.nodes.insert(
                id.clone(),
                Node {
                    info: info.clone(),
                    profile,
                    container,
                    log_path: self.cfg.state_dir.join("nodes").join(format!("{id}.jsonl")),
                    log_lines: 0,
                    output: String::new(),
                    state_tx,
                    log_tx,
                },
            );
            inner.tokens.insert(hash_token(&token), id.clone());
            (id, token, spec, info)
        };

        info!(node = %id, parent = %caller, dir = %info.dir, access = ?info.access, network = ?info.network, "spawn");

        let child = match self.docker.run(&spec, &token) {
            Ok(c) => c,
            Err(e) => {
                self.finish(&id, State::Failed, None, Some(format!("docker run: {e}")));
                return Err(SwarmError::new(
                    ErrorCode::RuntimeError,
                    format!("docker run: {e}"),
                ));
            }
        };

        let this = self.clone();
        let nid = id.clone();
        let timeout = Duration::from_secs(spec.profile.timeout_secs);
        tokio::spawn(async move {
            this.supervise(nid, child, timeout).await;
        });

        Ok((id, spec.name))
    }

    async fn supervise(&self, id: NodeId, mut child: tokio::process::Child, timeout: Duration) {
        let Some(log_path) = self.lock().nodes.get(&id).map(|n| n.log_path.clone()) else {
            return;
        };
        if let Some(dir) = log_path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(dir).await {
                warn!(node = %id, "cannot create log dir: {e}");
            }
        }
        let mut log = match tokio::fs::File::create(&log_path).await {
            Ok(f) => Some(f),
            Err(e) => {
                warn!(node = %id, "cannot create log file: {e}");
                None
            }
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(bool, String)>(1024);

        if let Some(out) = stdout {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(out).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    if tx.send((false, l)).await.is_err() {
                        break;
                    }
                }
            });
        }
        if let Some(err) = stderr {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(err).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    if tx.send((true, l)).await.is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);

        let deadline = tokio::time::Instant::now() + timeout;
        let mut timed_out = false;
        // After a stop, do not depend on the pipes closing: a process that
        // ignores SIGTERM and holds stdout must not stall supervision.
        let hard_deadline = tokio::time::Instant::now()
            + timeout
            + Duration::from_secs(u64::from(STOP_GRACE_SECS) + 5);

        // The pipes close when the container exits; that is the completion signal.
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some((is_err, line)) => {
                        let line = if is_err {
                            serde_json::json!({"type": "stderr", "line": line}).to_string()
                        } else {
                            self.ingest_event(&id, &line);
                            line
                        };
                        if let Some(f) = log.as_mut() {
                            if f.write_all(line.as_bytes()).await.is_err() || f.write_all(b"\n").await.is_err() {
                                log = None;
                            }
                        }
                        self.publish_line(&id, line);
                    }
                    None => break,
                },
                () = tokio::time::sleep_until(deadline), if !timed_out => {
                    timed_out = true;
                    warn!(node = %id, "timeout after {}s; stopping", timeout.as_secs());
                    let name = self.lock().nodes.get(&id).map(|n| n.container.clone()).unwrap_or_default();
                    let _ = self.docker.stop(&name, STOP_GRACE_SECS).await;
                    let _ = child.start_kill();
                }
                () = tokio::time::sleep_until(hard_deadline), if timed_out => {
                    warn!(node = %id, "output pipes still open after stop; abandoning them");
                    let _ = child.start_kill();
                    break;
                }
            }
        }

        let exit = child.wait().await.ok();
        if let Some(f) = log.as_mut() {
            let _ = f.flush().await;
        }

        let code = exit.and_then(|s| s.code());
        let already_terminal = self
            .lock()
            .nodes
            .get(&id)
            .is_none_or(|n| n.info.state.is_terminal());
        if !already_terminal {
            if timed_out {
                self.finish(&id, State::Stopped, code, Some("timeout".to_owned()));
            } else {
                self.finish(&id, State::Exited, code, None);
            }
        }
        debug!(node = %id, ?code, timed_out, "supervision finished");
    }

    fn publish_line(&self, id: &str, line: String) {
        let mut inner = self.lock();
        if let Some(n) = inner.nodes.get_mut(id) {
            let seq = n.log_lines;
            n.log_lines += 1;
            let _ = n.log_tx.send(LogLine {
                seq,
                line: Arc::from(line),
            });
        }
    }

    /// Fold one pi JSON event into the node's usage/output.
    fn ingest_event(&self, id: &str, line: &str) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        if v.get("type").and_then(Value::as_str) != Some("message_end") {
            return;
        }
        let Some(msg) = v.get("message") else { return };
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        let text = msg
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        let usage = msg.get("usage");
        let get = |k: &str| {
            usage
                .and_then(|u| u.get(k))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        };
        let cost = usage
            .and_then(|u| u.get("cost"))
            .and_then(|c| c.get("total"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let model = msg.get("model").and_then(Value::as_str).map(str::to_owned);

        let mut inner = self.lock();
        if let Some(n) = inner.nodes.get_mut(id) {
            n.info.usage.turns += 1;
            n.info.usage.input += get("input");
            n.info.usage.output += get("output");
            n.info.usage.cache_read += get("cacheRead");
            n.info.usage.cache_write += get("cacheWrite");
            n.info.usage.cost += cost;
            if model.is_some() {
                n.info.model = model;
            }
            if !text.is_empty() {
                n.output = text;
            }
            let _ = n.state_tx.send(n.info.clone());
        }
    }

    fn finish(&self, id: &str, state: State, code: Option<i32>, reason: Option<String>) {
        let mut inner = self.lock();
        let Some(n) = inner.nodes.get_mut(id) else {
            return;
        };
        if n.info.state.is_terminal() {
            return;
        }
        n.info.state = state;
        n.info.exit_code = code;
        n.info.reason = reason;
        n.info.finished_at = Some(now());
        let _ = n.state_tx.send(n.info.clone());
        info!(node = %id, ?state, ?code, "finished");
    }

    /// Stop `id` and every node below it. Returns the ids stopped.
    pub async fn stop_subtree(&self, id: &str, reason: &str) -> Vec<NodeId> {
        let targets: Vec<(NodeId, String)> = {
            let inner = self.lock();
            let mut ids: Vec<&NodeId> = inner
                .nodes
                .iter()
                .filter(|(nid, n)| {
                    n.info.state == State::Running
                        && nid.as_str() != "000"
                        && Self::is_descendant(&inner, id, nid)
                })
                .map(|(nid, _)| nid)
                .collect();
            ids.sort();
            ids.reverse(); // deepest (newest) first
            ids.iter()
                .filter_map(|nid| {
                    inner
                        .nodes
                        .get(*nid)
                        .map(|n| ((*nid).clone(), n.container.clone()))
                })
                .collect()
        };
        let mut stopped = Vec::new();
        for (nid, container) in targets {
            info!(node = %nid, reason, "stop");
            // Mark first so the supervisor does not record it as a plain exit.
            self.finish(&nid, State::Stopped, None, Some(reason.to_owned()));
            let _ = self.docker.stop(&container, STOP_GRACE_SECS).await;
            stopped.push(nid);
        }
        stopped
    }

    /// Refuse new spawns and stop everything.
    pub async fn shutdown(&self, reason: &str) -> Vec<NodeId> {
        self.lock().shutting_down = true;
        self.stop_subtree("000", reason).await
    }

    /// True after [`Swarm::shutdown`].
    pub fn is_shutting_down(&self) -> bool {
        self.lock().shutting_down
    }

    /// Read the first `upto` lines of a node's log from disk.
    pub async fn read_log(path: &Path, upto: u64) -> io::Result<Vec<String>> {
        let content = match tokio::fs::read_to_string(path).await {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        Ok(content
            .lines()
            .take(usize::try_from(upto).unwrap_or(usize::MAX))
            .map(str::to_owned)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Network;

    #[test]
    fn tokens_are_unique_hex() {
        let a = new_token();
        let b = new_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), TOKEN_BYTES * 2);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn preview_truncates_and_marks_multiline() {
        assert_eq!(preview("short"), "short");
        assert_eq!(preview("a\nb"), "a…");
        let long = "x".repeat(200);
        assert_eq!(preview(&long).chars().count(), TASK_PREVIEW_CHARS + 1);
    }

    fn test_swarm() -> (Swarm, String, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = SwarmConfig {
            id: "t".into(),
            state_dir: tmp.path().to_path_buf(),
            image: "img".into(),
            config_volume: "vol".into(),
            transport: Transport::Unix(tmp.path().join("s")),
            user: None,
            limits: Limits {
                max_children: 2,
                max_depth: 2,
                max_total: 8,
            },
            extra_docker_args: vec![],
        };
        let root = Profile {
            host_dir: tmp.path().canonicalize().unwrap(),
            access: Access::ReadWrite,
            network: Network::Full,
            timeout_secs: 60,
            cpus: 1.0,
            memory_mib: 512,
            depth: 0,
        };
        let (s, tok) = Swarm::new(cfg, Docker::new("/nonexistent/docker"), root);
        (s, tok, tmp)
    }

    #[test]
    fn root_authenticates_and_sees_nothing() {
        let (s, tok, _tmp) = test_swarm();
        assert_eq!(s.authenticate(&tok).unwrap_or_default(), "000");
        assert_eq!(
            s.authenticate("bogus").err().map(|e| e.code),
            Some(ErrorCode::Unauthorized)
        );
        assert!(s.list("000").is_empty());
        assert_eq!(
            s.authorize("000", "000").err().map(|e| e.code),
            Some(ErrorCode::Forbidden)
        );
        assert_eq!(
            s.authorize("000", "999").err().map(|e| e.code),
            Some(ErrorCode::NotFound)
        );
    }

    #[test]
    fn spawn_with_missing_docker_fails_cleanly() {
        let (s, _tok, _tmp) = test_swarm();
        let params = SpawnParams {
            task: "t".into(),
            ..SpawnParams::default()
        };
        let err = s.spawn("000", &params).err().unwrap();
        assert_eq!(err.code, ErrorCode::RuntimeError);
        // The node exists, marked failed, visible to root.
        let nodes = s.list("000");
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].state, State::Failed);
    }

    #[test]
    fn empty_task_is_bad_request() {
        let (s, _tok, _tmp) = test_swarm();
        let params = SpawnParams {
            task: "  ".into(),
            ..SpawnParams::default()
        };
        assert_eq!(
            s.spawn("000", &params).err().map(|e| e.code),
            Some(ErrorCode::BadRequest)
        );
    }
}
