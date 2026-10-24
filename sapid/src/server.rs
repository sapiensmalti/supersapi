//! Socket server: accepts connections, authenticates each request, dispatches
//! to the [`Swarm`], and writes reply frames.
//!
//! Each connection is handled by one task; each request on it by another, so
//! a long `wait` never blocks a `list`. Writes are serialised through a
//! channel so frames never interleave.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::protocol::{
    ErrorCode, Frame, HelloResult, ListResult, LogsParams, NodeInfo, Op, Request, SpawnResult,
    StopResult, WaitParams, WaitResult, VERSION,
};
use crate::swarm::{LogLine, Swarm, SwarmError};

/// Maximum accepted request line, in bytes. Tasks are prompts, not files.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Event types forwarded to a streaming `wait`. Everything else is noise for
/// a parent that wants to know what its child is doing.
const STREAMED_EVENTS: &[&str] = &[
    "tool_execution_start",
    "tool_execution_end",
    "message_end",
    "stderr",
];

/// A bound listener of either flavour.
pub enum Listener {
    /// Unix domain socket.
    Unix(UnixListener),
    /// TCP socket.
    Tcp(TcpListener),
}

impl Listener {
    /// Bind a Unix socket at `path` with mode 0600, replacing a stale file.
    pub fn bind_unix(path: &Path) -> std::io::Result<Self> {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let l = UnixListener::bind(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Containers connect as whatever uid Docker maps them to; on Linux
            // that is the host user (or root via the daemon). Owner-only.
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(Listener::Unix(l))
    }

    /// Bind a TCP socket.
    pub async fn bind_tcp(addr: &str) -> std::io::Result<Self> {
        Ok(Listener::Tcp(TcpListener::bind(addr).await?))
    }

    /// Local TCP port, if TCP.
    pub fn tcp_port(&self) -> Option<u16> {
        match self {
            Listener::Tcp(l) => l.local_addr().ok().map(|a| a.port()),
            Listener::Unix(_) => None,
        }
    }
}

/// Accept loop. Returns when `shutdown` fires.
pub async fn serve(
    listener: Listener,
    swarm: Swarm,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let swarm = Arc::new(swarm);
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            accepted = accept(&listener) => match accepted {
                Ok(stream) => {
                    let swarm = Arc::clone(&swarm);
                    tokio::spawn(async move { handle_connection(stream, swarm).await });
                }
                Err(e) => {
                    warn!("accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }
    info!("listener closed");
}

/// Erased stream type.
type Stream = Box<dyn AsyncReadWrite + Send + Unpin>;

/// Marker trait to box either stream kind.
pub trait AsyncReadWrite: AsyncRead + AsyncWrite {}
impl<T: AsyncRead + AsyncWrite> AsyncReadWrite for T {}

async fn accept(listener: &Listener) -> std::io::Result<Stream> {
    match listener {
        Listener::Unix(l) => l.accept().await.map(|(s, _)| Box::new(s) as Stream),
        Listener::Tcp(l) => l.accept().await.map(|(s, _)| Box::new(s) as Stream),
    }
}

async fn handle_connection(stream: Stream, swarm: Arc<Swarm>) {
    let (rd, mut wr) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::channel::<Frame>(256);

    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let Ok(mut line) = serde_json::to_vec(&frame) else {
                continue;
            };
            line.push(b'\n');
            if wr.write_all(&line).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });

    let mut lines = BufReader::with_capacity(64 * 1024, rd).lines();
    loop {
        let line = match read_bounded_line(&mut lines).await {
            Ok(Some(l)) => l,
            Ok(None) => break,
            Err(e) => {
                let _ = tx.send(Frame::err("", ErrorCode::BadRequest, e)).await;
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let id = serde_json::from_str::<Value>(&line)
                    .ok()
                    .and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_owned))
                    .unwrap_or_default();
                let _ = tx
                    .send(Frame::err(
                        &id,
                        ErrorCode::BadRequest,
                        format!("malformed request: {e}"),
                    ))
                    .await;
                continue;
            }
        };
        let swarm = Arc::clone(&swarm);
        let tx = tx.clone();
        tokio::spawn(async move {
            let id = req.id.clone();
            let frame = match dispatch(req, &swarm, &tx).await {
                Ok(frame) => frame,
                Err(e) => Frame::err(&id, e.code, e.message),
            };
            let _ = tx.send(frame).await;
        });
    }
    drop(tx);
    let _ = writer.await;
    debug!("connection closed");
}

async fn read_bounded_line<R: AsyncRead + Unpin>(
    lines: &mut tokio::io::Lines<BufReader<R>>,
) -> Result<Option<String>, String> {
    match lines.next_line().await {
        Ok(Some(l)) if l.len() > MAX_LINE_BYTES => {
            Err(format!("request exceeds {MAX_LINE_BYTES} bytes"))
        }
        Ok(l) => Ok(l),
        Err(e) => Err(format!("read error: {e}")),
    }
}

async fn dispatch(
    req: Request,
    swarm: &Swarm,
    tx: &mpsc::Sender<Frame>,
) -> Result<Frame, SwarmError> {
    let caller = swarm.authenticate(&req.token)?;
    let id = req.id.as_str();
    match req.op {
        Op::Hello => {
            let node = swarm.info(&caller).ok_or_else(|| SwarmError {
                code: ErrorCode::Unauthorized,
                message: "caller vanished".into(),
            })?;
            Ok(Frame::ok(
                id,
                &HelloResult {
                    protocol: VERSION,
                    server: env!("CARGO_PKG_VERSION").to_owned(),
                    node,
                    limits: swarm.limits(),
                },
            ))
        }
        Op::Spawn(params) => {
            let (nid, container) = swarm.spawn(&caller, &params)?;
            Ok(Frame::ok(id, &SpawnResult { id: nid, container }))
        }
        Op::List => Ok(Frame::ok(
            id,
            &ListResult {
                nodes: swarm.list(&caller),
            },
        )),
        Op::Status(r) => {
            swarm.authorize(&caller, &r.id)?;
            let node = swarm.info(&r.id).ok_or_else(|| not_found(&r.id))?;
            Ok(Frame::ok(id, &node))
        }
        Op::Wait(p) => {
            swarm.authorize(&caller, &p.id)?;
            wait(id, &p, swarm, tx).await
        }
        Op::Logs(p) => {
            swarm.authorize(&caller, &p.id)?;
            logs(id, &p, swarm, tx).await
        }
        Op::Stop(r) => {
            swarm.authorize(&caller, &r.id)?;
            let stopped = swarm
                .stop_subtree(&r.id, &format!("stopped by {caller}"))
                .await;
            Ok(Frame::ok(id, &StopResult { stopped }))
        }
        Op::Shutdown => {
            if caller != "000" {
                return Err(SwarmError {
                    code: ErrorCode::Forbidden,
                    message: "shutdown is root-only".into(),
                });
            }
            let stopped = swarm.shutdown("shutdown").await;
            Ok(Frame::ok(id, &StopResult { stopped }))
        }
    }
}

fn not_found(id: &str) -> SwarmError {
    SwarmError {
        code: ErrorCode::NotFound,
        message: format!("no node {id}"),
    }
}

async fn wait(
    id: &str,
    p: &WaitParams,
    swarm: &Swarm,
    tx: &mpsc::Sender<Frame>,
) -> Result<Frame, SwarmError> {
    let (mut state_rx, mut log_rx, _, _) =
        swarm.subscribe(&p.id).ok_or_else(|| not_found(&p.id))?;
    let deadline = p
        .timeout_secs
        .map(|s| tokio::time::Instant::now() + Duration::from_secs(s));
    let mut timed_out = false;

    loop {
        let current: NodeInfo = state_rx.borrow().clone();
        if current.state.is_terminal() {
            break;
        }
        tokio::select! {
            changed = state_rx.changed() => {
                if changed.is_err() { break; }
            }
            line = log_rx.recv(), if p.stream => match line {
                Ok(LogLine { line, .. }) => {
                    if let Some(ev) = filter_event(&line) {
                        let _ = tx.send(Frame::event(id, ev)).await;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    let _ = tx.send(Frame::event(id, serde_json::json!({"type": "lagged", "skipped": n}))).await;
                }
                Err(broadcast::error::RecvError::Closed) => {}
            },
            () = async { if let Some(d) = deadline { tokio::time::sleep_until(d).await } else { std::future::pending().await } } => {
                timed_out = true;
                break;
            }
        }
    }

    let node = swarm.info(&p.id).ok_or_else(|| not_found(&p.id))?;
    let output = swarm.output(&p.id);
    Ok(Frame::ok(
        id,
        &WaitResult {
            node,
            output,
            timed_out,
        },
    ))
}

fn filter_event(line: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(line).ok()?;
    let t = v.get("type")?.as_str()?;
    if !STREAMED_EVENTS.contains(&t) {
        return None;
    }
    if t == "message_end" {
        let msg = v.get("message")?;
        if msg.get("role")?.as_str()? != "assistant" {
            return None;
        }
        let text: String = msg
            .get("content")?
            .as_array()?
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect();
        return Some(serde_json::json!({"type": "assistant", "text": text}));
    }
    if t.starts_with("tool_execution") {
        return Some(serde_json::json!({
            "type": t,
            "tool": v.get("toolName"),
            "args": v.get("args"),
            "is_error": v.get("isError"),
        }));
    }
    Some(v)
}

async fn logs(
    id: &str,
    p: &LogsParams,
    swarm: &Swarm,
    tx: &mpsc::Sender<Frame>,
) -> Result<Frame, SwarmError> {
    let (mut state_rx, mut log_rx, written, path) =
        swarm.subscribe(&p.id).ok_or_else(|| not_found(&p.id))?;
    // Subscribe first, then read the file: lines with seq < written are
    // already on disk and must not be delivered twice.
    let existing = Swarm::read_log(&path, written).await.unwrap_or_default();
    let mut delivered: u64 = 0;
    for line in existing {
        let _ = tx
            .send(Frame::event(id, serde_json::json!({"line": line})))
            .await;
        delivered += 1;
    }
    if p.follow {
        loop {
            if state_rx.borrow().state.is_terminal() && log_rx.is_empty() {
                break;
            }
            tokio::select! {
                changed = state_rx.changed() => { if changed.is_err() { break; } }
                line = log_rx.recv() => match line {
                    Ok(LogLine { seq, line }) => {
                        if seq >= delivered {
                            let _ = tx.send(Frame::event(id, serde_json::json!({"line": &*line}))).await;
                            delivered = seq + 1;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    Ok(Frame::ok(id, &serde_json::json!({"lines": delivered})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_keeps_assistant_text_and_tools_only() {
        let a = filter_event(
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"hi"},{"type":"thinking","thinking":"x"}]}}"#,
        );
        assert_eq!(a, Some(serde_json::json!({"type":"assistant","text":"hi"})));
        assert!(
            filter_event(r#"{"type":"message_end","message":{"role":"user","content":"x"}}"#)
                .is_none()
        );
        assert!(filter_event(r#"{"type":"message_update","x":1}"#).is_none());
        let t = filter_event(
            r#"{"type":"tool_execution_start","toolName":"bash","args":{"command":"ls"}}"#,
        );
        assert_eq!(
            t.and_then(|v| v.get("tool").cloned()),
            Some(serde_json::json!("bash"))
        );
        assert!(filter_event("not json").is_none());
    }
}
