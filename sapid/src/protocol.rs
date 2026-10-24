//! Wire protocol between agents and the broker.
//!
//! Framing is JSON Lines: one JSON object per `\n`-terminated line, in both
//! directions. A client sends [`Request`]s; the broker answers each one with
//! zero or more [`Frame::Event`]s followed by exactly one [`Frame::Ok`] or
//! [`Frame::Err`] carrying the same `id`. Requests on one connection may be
//! pipelined and are answered independently.
//!
//! The protocol is deliberately small. Every operation is scoped by the
//! caller's token: a node sees and controls only its own descendants.

use serde::{Deserialize, Serialize};

/// Protocol version spoken by this build.
pub const VERSION: u32 = 1;

/// A client request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Client-chosen correlation id, echoed on every frame of the reply.
    pub id: String,
    /// Capability token of the calling node.
    pub token: String,
    /// The operation and its parameters.
    #[serde(flatten)]
    pub op: Op,
}

/// Operations, tagged by `op`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", content = "params", rename_all = "snake_case")]
pub enum Op {
    /// Identify the caller and report the broker's limits.
    Hello,
    /// Spawn a child agent.
    Spawn(SpawnParams),
    /// List the caller's descendants.
    List,
    /// Describe one descendant.
    Status(NodeRef),
    /// Block until a descendant finishes. Streams progress events when `stream` is set.
    Wait(WaitParams),
    /// Return (and optionally follow) a descendant's raw JSONL log.
    Logs(LogsParams),
    /// Stop a descendant and its whole subtree.
    Stop(NodeRef),
    /// Stop every node and exit. Root only.
    Shutdown,
}

/// Refers to a node by id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRef {
    /// Node id as returned by `spawn`.
    pub id: String,
}

/// Parameters for `wait`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitParams {
    /// Node id.
    pub id: String,
    /// Forward the child's progress as `event` frames while waiting.
    #[serde(default)]
    pub stream: bool,
    /// Give up after this many seconds (the child keeps running).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Parameters for `logs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogsParams {
    /// Node id.
    pub id: String,
    /// Keep streaming until the node exits.
    #[serde(default)]
    pub follow: bool,
}

/// How a child may touch the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// No workspace at all: an empty, discarded scratch volume.
    None,
    /// The directory is mounted read-only.
    ReadOnly,
    /// The directory is mounted read-write.
    #[default]
    ReadWrite,
}

/// Network reach of a child.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Network {
    /// No network interface besides loopback.
    None,
    /// Normal outbound network (needed to reach model providers).
    #[default]
    Full,
}

/// Parameters for `spawn`.
///
/// Unset fields inherit from the caller. Set fields may only narrow what the
/// caller has; anything wider is rejected (see `policy`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SpawnParams {
    /// The task, given to the child verbatim as its prompt.
    pub task: String,
    /// Directory to mount, relative to the caller's own workspace. Default: `.`.
    #[serde(default)]
    pub dir: Option<String>,
    /// Filesystem access. Default: inherit.
    #[serde(default)]
    pub access: Option<Access>,
    /// Network reach. Default: inherit.
    #[serde(default)]
    pub network: Option<Network>,
    /// Model id passed to `pi --model`. Default: pi decides.
    #[serde(default)]
    pub model: Option<String>,
    /// Wall-clock budget in seconds. Default: inherit.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// CPU quota (`docker --cpus`). Default: inherit.
    #[serde(default)]
    pub cpus: Option<f64>,
    /// Memory limit in MiB. Default: inherit.
    #[serde(default)]
    pub memory_mib: Option<u64>,
    /// Human label shown in listings.
    #[serde(default)]
    pub name: Option<String>,
    /// Trust project-local pi files (`.pi/`) in the mounted directory. Default: false.
    #[serde(default)]
    pub approve_project: bool,
}

/// Lifecycle state of a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// The container is being created or is running.
    Running,
    /// The process exited on its own.
    Exited,
    /// Stopped by a `stop`, a timeout, or shutdown.
    Stopped,
    /// The broker could not run it.
    Failed,
}

impl State {
    /// True once the node will never change again.
    pub fn is_terminal(self) -> bool {
        !matches!(self, State::Running)
    }
}

/// Token accounting aggregated from the child's assistant messages.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Assistant turns.
    pub turns: u32,
    /// Input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Cache-read tokens.
    pub cache_read: u64,
    /// Cache-write tokens.
    pub cache_write: u64,
    /// Provider-reported cost, in USD.
    pub cost: f64,
}

/// Public description of a node. Host paths are never included; `dir` is
/// relative to the swarm root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    /// Node id.
    pub id: String,
    /// Parent id; `None` for the root.
    pub parent: Option<String>,
    /// Depth below the root (root is 0).
    pub depth: u32,
    /// Label.
    pub name: String,
    /// First line of the task, truncated.
    pub task: String,
    /// Mounted directory, relative to the swarm root (`.` for the root itself).
    pub dir: String,
    /// Filesystem access.
    pub access: Access,
    /// Network reach.
    pub network: Network,
    /// Wall-clock budget in seconds.
    pub timeout_secs: u64,
    /// Lifecycle state.
    pub state: State,
    /// Exit code once exited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Why the node failed or was stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Unix seconds when it started.
    pub started_at: u64,
    /// Unix seconds when it finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    /// Aggregated usage.
    pub usage: Usage,
    /// Model observed in the child's messages, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Result of `hello`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloResult {
    /// Protocol version.
    pub protocol: u32,
    /// Broker version.
    pub server: String,
    /// The caller's own node.
    pub node: NodeInfo,
    /// Limits in force for the caller.
    pub limits: Limits,
}

/// Fan-out and depth limits as seen by one node.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Limits {
    /// Maximum children this node may have running at once.
    pub max_children: u32,
    /// Maximum depth of the tree below the root.
    pub max_depth: u32,
    /// Maximum nodes alive across the whole swarm.
    pub max_total: u32,
}

/// Result of `spawn`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnResult {
    /// The new node's id.
    pub id: String,
    /// Its container name on the host, for debugging with plain `docker`.
    pub container: String,
}

/// Result of `wait`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitResult {
    /// Final node description.
    pub node: NodeInfo,
    /// The child's last assistant message, as text.
    pub output: String,
    /// True if the wait timed out before the node finished.
    pub timed_out: bool,
}

/// Result of `list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListResult {
    /// Descendants of the caller, depth-first.
    pub nodes: Vec<NodeInfo>,
}

/// Result of `stop` and `shutdown`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopResult {
    /// Ids that were stopped.
    pub stopped: Vec<String>,
}

/// Machine-readable error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Malformed request.
    BadRequest,
    /// Unknown or revoked token.
    Unauthorized,
    /// The node exists but is not in the caller's subtree.
    Forbidden,
    /// No such node.
    NotFound,
    /// A policy rule rejected the spawn.
    PolicyViolation,
    /// A fan-out, depth, or total limit was hit.
    LimitExceeded,
    /// The container runtime failed.
    RuntimeError,
    /// The broker is shutting down.
    ShuttingDown,
}

/// Error payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    /// Code.
    pub code: ErrorCode,
    /// Human-readable explanation.
    pub message: String,
}

/// One line sent by the broker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Frame {
    /// Intermediate event for a streaming operation.
    Event {
        /// Request id.
        id: String,
        /// Payload.
        event: serde_json::Value,
    },
    /// Successful completion.
    Ok {
        /// Request id.
        id: String,
        /// Always `true`.
        ok: bool,
        /// Payload.
        result: serde_json::Value,
    },
    /// Failure.
    Err {
        /// Request id.
        id: String,
        /// Always `false`.
        ok: bool,
        /// Error.
        error: ErrorBody,
    },
}

impl Frame {
    /// Build a success frame.
    pub fn ok<T: Serialize>(id: &str, result: &T) -> Self {
        let result = serde_json::to_value(result).unwrap_or(serde_json::Value::Null);
        Frame::Ok {
            id: id.to_owned(),
            ok: true,
            result,
        }
    }

    /// Build an error frame.
    pub fn err(id: &str, code: ErrorCode, message: impl Into<String>) -> Self {
        Frame::Err {
            id: id.to_owned(),
            ok: false,
            error: ErrorBody {
                code,
                message: message.into(),
            },
        }
    }

    /// Build an event frame.
    pub fn event(id: &str, event: serde_json::Value) -> Self {
        Frame::Event {
            id: id.to_owned(),
            event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_request_round_trips() {
        let json =
            r#"{"id":"1","token":"t","op":"spawn","params":{"task":"do it","access":"read_only"}}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        match req.op {
            Op::Spawn(p) => {
                assert_eq!(p.task, "do it");
                assert_eq!(p.access, Some(Access::ReadOnly));
                assert_eq!(p.dir, None);
            }
            other => panic!("wrong op: {other:?}"),
        }
    }

    #[test]
    fn hello_has_no_params() {
        let req: Request = serde_json::from_str(r#"{"id":"1","token":"t","op":"hello"}"#).unwrap();
        assert!(matches!(req.op, Op::Hello));
    }

    #[test]
    fn access_orders_from_least_to_most() {
        assert!(Access::None < Access::ReadOnly);
        assert!(Access::ReadOnly < Access::ReadWrite);
        assert!(Network::None < Network::Full);
    }

    #[test]
    fn frames_serialize_with_id_and_ok() {
        let ok = serde_json::to_string(&Frame::ok("7", &serde_json::json!({"a": 1})))
            .unwrap_or_default();
        assert_eq!(ok, r#"{"id":"7","ok":true,"result":{"a":1}}"#);
        let err = serde_json::to_string(&Frame::err("7", ErrorCode::NotFound, "nope"))
            .unwrap_or_default();
        assert_eq!(
            err,
            r#"{"id":"7","ok":false,"error":{"code":"not_found","message":"nope"}}"#
        );
    }
}
