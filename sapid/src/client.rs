//! A minimal client, used by `sapid ctl` and by the integration tests.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixStream};

use crate::protocol::{Frame, Op, Request};
use crate::server::AsyncReadWrite;

/// Parsed `SAPID_ADDR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Addr {
    /// `unix:///path/to/sock`
    Unix(std::path::PathBuf),
    /// `tcp://host:port`
    Tcp(String),
}

impl Addr {
    /// Parse `unix://PATH` or `tcp://HOST:PORT`.
    pub fn parse(s: &str) -> Result<Self> {
        if let Some(p) = s.strip_prefix("unix://") {
            return Ok(Addr::Unix(p.into()));
        }
        if let Some(a) = s.strip_prefix("tcp://") {
            return Ok(Addr::Tcp(a.to_owned()));
        }
        Err(anyhow!("address must start with unix:// or tcp://: {s}"))
    }

    /// Render back to the canonical string form.
    pub fn to_url(&self) -> String {
        match self {
            Addr::Unix(p) => format!("unix://{}", p.display()),
            Addr::Tcp(a) => format!("tcp://{a}"),
        }
    }
}

/// An open connection.
pub struct Client {
    rd: BufReader<tokio::io::ReadHalf<Box<dyn AsyncReadWrite + Send + Unpin>>>,
    wr: tokio::io::WriteHalf<Box<dyn AsyncReadWrite + Send + Unpin>>,
    token: String,
    next: u64,
}

impl Client {
    /// Connect.
    pub async fn connect(addr: &Addr, token: String) -> Result<Self> {
        let stream: Box<dyn AsyncReadWrite + Send + Unpin> = match addr {
            Addr::Unix(p) => Box::new(
                UnixStream::connect(p)
                    .await
                    .with_context(|| format!("connect {}", p.display()))?,
            ),
            Addr::Tcp(a) => Box::new(
                TcpStream::connect(a)
                    .await
                    .with_context(|| format!("connect {a}"))?,
            ),
        };
        let (rd, wr) = tokio::io::split(stream);
        Ok(Self {
            rd: BufReader::new(rd),
            wr,
            token,
            next: 1,
        })
    }

    /// Send one request and call `on_event` for each event frame; return the
    /// final frame.
    pub async fn call(
        &mut self,
        op: Op,
        mut on_event: impl FnMut(&serde_json::Value),
    ) -> Result<Frame> {
        let id = self.next.to_string();
        self.next += 1;
        let req = Request {
            id: id.clone(),
            token: self.token.clone(),
            op,
        };
        let mut line = serde_json::to_vec(&req)?;
        line.push(b'\n');
        self.wr.write_all(&line).await?;
        let mut buf = String::new();
        loop {
            buf.clear();
            if self.rd.read_line(&mut buf).await? == 0 {
                return Err(anyhow!("connection closed before reply"));
            }
            let frame: Frame =
                serde_json::from_str(buf.trim_end()).context("bad frame from broker")?;
            match &frame {
                Frame::Event { id: fid, event } if *fid == id => on_event(event),
                Frame::Ok { id: fid, .. } | Frame::Err { id: fid, .. } if *fid == id => {
                    return Ok(frame)
                }
                _ => {}
            }
        }
    }
}

/// Read a token file, trimming whitespace.
pub fn read_token(path: &Path) -> Result<String> {
    Ok(std::fs::read_to_string(path)
        .with_context(|| format!("read token {}", path.display()))?
        .trim()
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addr_parses_both_forms() {
        assert_eq!(
            Addr::parse("unix:///a/b").ok(),
            Some(Addr::Unix("/a/b".into()))
        );
        assert_eq!(Addr::parse("tcp://h:1").ok(), Some(Addr::Tcp("h:1".into())));
        assert!(Addr::parse("http://x").is_err());
        assert_eq!(Addr::Unix("/a".into()).to_url(), "unix:///a");
    }
}
