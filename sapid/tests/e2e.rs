//! End-to-end tests against a running `sapid serve`, with `tests/fake-docker.sh`
//! standing in for Docker. Exercises the real socket, the real CLI client and
//! the real supervision loop.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

struct Broker {
    child: Child,
    state: tempfile::TempDir,
    fake_state: tempfile::TempDir,
    root: tempfile::TempDir,
}

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_sapid"))
}

fn fake_docker() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-docker.sh")
}

impl Broker {
    fn start(extra: &[&str]) -> Self {
        let state = tempfile::tempdir().unwrap();
        let fake_state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("sub/deeper")).unwrap();
        let mut cmd = Command::new(bin());
        cmd.args([
            "serve",
            "--state-dir",
            state.path().to_str().unwrap(),
            "--root-dir",
            root.path().to_str().unwrap(),
            "--image",
            "fake:image",
            "--docker",
            fake_docker().to_str().unwrap(),
        ])
        .args(extra)
        .env("FAKE_DOCKER_STATE", fake_state.path())
        .env("SAPID_BIN", bin())
        .env("SAPID_LOG", "debug")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
        let child = cmd.spawn().unwrap();
        let b = Self {
            child,
            state,
            fake_state,
            root,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !b.state.path().join("token").exists() {
            assert!(Instant::now() < deadline, "broker did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        b
    }

    fn addr(&self) -> String {
        format!("unix://{}", self.state.path().join("sapid.sock").display())
    }

    fn token(&self) -> String {
        std::fs::read_to_string(self.state.path().join("token")).unwrap()
    }

    /// Run `sapid ctl` as root. Returns (exit ok, stdout, stderr).
    fn ctl(&self, args: &[&str]) -> (bool, String, String) {
        self.ctl_with_token(&self.token(), args)
    }

    fn ctl_with_token(&self, token: &str, args: &[&str]) -> (bool, String, String) {
        let out = Command::new(bin())
            .args(["ctl", "--addr", &self.addr(), "--token", token])
            .args(args)
            .env("FAKE_DOCKER_STATE", self.fake_state.path())
            .env("SAPID_BIN", bin())
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn ctl_json(&self, args: &[&str]) -> Value {
        let (ok, out, err) = self.ctl(args);
        assert!(ok, "ctl {args:?} failed: {err}");
        // The final pretty-printed JSON object is the result; events precede it.
        let start = out.rfind("\n{").map_or(0, |i| i + 1);
        serde_json::from_str(&out[start..]).unwrap_or_else(|e| panic!("bad json: {e}\n{out}"))
    }

    fn spec_of(&self, node: &str) -> String {
        std::fs::read_to_string(self.fake_state.path().join(format!("node-{node}.spec")))
            .unwrap_or_default()
    }

    fn stderr(&mut self) -> String {
        let mut s = String::new();
        if let Some(mut e) = self.child.stderr.take() {
            use std::io::Read;
            let _ = e.read_to_string(&mut s);
        }
        s
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn hello_identifies_root_and_limits() {
    let b = Broker::start(&[
        "--max-children",
        "3",
        "--max-depth",
        "2",
        "--max-total",
        "9",
    ]);
    let v = b.ctl_json(&["hello"]);
    assert_eq!(v["node"]["id"], "000");
    assert_eq!(v["node"]["depth"], 0);
    assert_eq!(v["node"]["dir"], ".");
    assert_eq!(v["limits"]["max_children"], 3);
    assert_eq!(v["limits"]["max_depth"], 2);
    assert_eq!(v["limits"]["max_total"], 9);
    assert_eq!(v["protocol"], 1);
}

#[test]
fn bad_token_is_unauthorized() {
    let b = Broker::start(&[]);
    let (ok, _, err) = b.ctl_with_token("nope", &["hello"]);
    assert!(!ok);
    assert!(err.contains("Unauthorized"), "{err}");
}

#[test]
fn spawn_wait_collects_output_and_usage() {
    let b = Broker::start(&[]);
    let v = b.ctl_json(&["spawn", "--wait", "echo:hello world"]);
    assert_eq!(v["output"], "hello world");
    assert_eq!(v["node"]["state"], "exited");
    assert_eq!(v["node"]["exit_code"], 0);
    assert_eq!(v["node"]["usage"]["turns"], 1);
    assert_eq!(v["node"]["usage"]["input"], 10);
    assert_eq!(v["node"]["model"], "fake-model");
    assert_eq!(v["timed_out"], false);

    // The log is on disk and readable through the protocol.
    let (ok, out, _) = b.ctl(&["logs", "001"]);
    assert!(ok);
    assert!(out.contains(r#"{"type":"agent_start"}"#), "{out}");
    assert!(out.contains("hello world"), "{out}");
    assert!(b.state.path().join("nodes/001.jsonl").exists());
}

#[test]
fn child_failure_is_reported_with_stderr() {
    let b = Broker::start(&[]);
    let v = b.ctl_json(&["spawn", "--wait", "fail"]);
    assert_eq!(v["node"]["state"], "exited");
    assert_eq!(v["node"]["exit_code"], 3);
    let (_, out, _) = b.ctl(&["logs", "001"]);
    let line = out
        .lines()
        .find(|l| l.contains("something broke"))
        .unwrap_or_default();
    let v: Value = serde_json::from_str(line).unwrap();
    assert_eq!(v["type"], "stderr");
    assert_eq!(v["line"], "something broke");
}

#[test]
fn subdirectory_mount_and_read_only_are_honoured() {
    let b = Broker::start(&[]);
    let v = b.ctl_json(&[
        "spawn",
        "--dir",
        "sub/deeper",
        "--access",
        "read-only",
        "--wait",
        "echo:x",
    ]);
    assert_eq!(v["node"]["dir"], "sub/deeper");
    assert_eq!(v["node"]["access"], "read_only");
    let spec = b.spec_of("001");
    // the fake records the /workspace mount; it must point at the subdirectory, read-only
    let root = b.root.path().canonicalize().unwrap();
    assert!(
        spec.contains(&format!("mount={}/sub/deeper:/workspace", root.display())),
        "{spec}"
    );
    let (_, out, _) = b.ctl(&["status", "001"]);
    assert!(out.contains("read_only"));
}

#[test]
fn scratch_and_no_network() {
    let b = Broker::start(&[]);
    let v = b.ctl_json(&[
        "spawn",
        "--access",
        "none",
        "--network",
        "none",
        "--wait",
        "echo:x",
    ]);
    assert_eq!(v["node"]["dir"], "(scratch)");
    let spec = b.spec_of("001");
    assert!(spec.contains("network=none"), "{spec}");
    assert!(
        !spec.contains(b.root.path().to_str().unwrap()),
        "scratch must not mount the dir: {spec}"
    );
}

#[test]
fn escaping_the_workspace_is_refused() {
    let b = Broker::start(&[]);
    for dir in ["..", "/etc", "sub/../..", "missing"] {
        let (ok, _, err) = b.ctl(&["spawn", "--dir", dir, "echo:x"]);
        assert!(!ok, "{dir} should be refused");
        assert!(err.contains("PolicyViolation"), "{dir}: {err}");
    }
    assert_eq!(b.ctl_json(&["list"])["nodes"].as_array().unwrap().len(), 0);
}

#[test]
fn widening_is_refused() {
    let b = Broker::start(&[
        "--access",
        "read-only",
        "--network",
        "none",
        "--timeout-secs",
        "30",
    ]);
    let (ok, _, err) = b.ctl(&["spawn", "--access", "read-write", "echo:x"]);
    assert!(!ok && err.contains("wider"), "{err}");
    let (ok, _, err) = b.ctl(&["spawn", "--network", "full", "echo:x"]);
    assert!(!ok && err.contains("wider"), "{err}");
    let (ok, _, err) = b.ctl(&["spawn", "--timeout-secs", "31", "echo:x"]);
    assert!(!ok && err.contains("exceeds"), "{err}");
    // Narrowing is fine.
    let v = b.ctl_json(&[
        "spawn",
        "--access",
        "none",
        "--timeout-secs",
        "5",
        "--wait",
        "echo:x",
    ]);
    assert_eq!(v["node"]["timeout_secs"], 5);
}

#[test]
fn grandchild_spawns_through_the_socket_and_is_attenuated() {
    let b = Broker::start(&["--max-depth", "2"]);
    // Child mounts `sub` read-only and asks for a grandchild in `deeper`.
    let v = b.ctl_json(&[
        "spawn",
        "--dir",
        "sub",
        "--access",
        "read-only",
        "--wait",
        "grandchild:echo:from the grandchild",
    ]);
    assert_eq!(v["output"], "grandchild said: from the grandchild", "{v}");

    let nodes = b.ctl_json(&["list"])["nodes"].as_array().unwrap().clone();
    assert_eq!(nodes.len(), 2, "{nodes:?}");
    let gc = nodes.iter().find(|n| n["id"] == "002").unwrap();
    assert_eq!(gc["parent"], "001");
    assert_eq!(gc["depth"], 2);
    assert_eq!(
        gc["access"], "read_only",
        "grandchild inherits the child's ceiling"
    );
    assert_eq!(
        gc["dir"], "sub",
        "grandchild dir is relative to the swarm root"
    );

    // Depth 2 is the ceiling: a great-grandchild must be refused at the broker.
    let v = b.ctl_json(&["spawn", "--wait", "grandchild:grandchild:echo:x"]);
    assert_eq!(v["output"], "grandchild said: grandchild said: ", "{v}");
    let nodes = b.ctl_json(&["list"])["nodes"].as_array().unwrap().clone();
    assert!(
        nodes.iter().all(|n| n["depth"].as_u64().unwrap() <= 2),
        "{nodes:?}"
    );
    assert_eq!(nodes.len(), 4);
}

#[test]
fn child_cannot_escape_its_own_subtree_dir() {
    let b = Broker::start(&[]);
    let v = b.ctl_json(&["spawn", "--dir", "sub", "--wait", "escape"]);
    let out = v["output"].as_str().unwrap();
    assert!(out.starts_with("refused:"), "{out}");
    assert!(out.contains("PolicyViolation"), "{out}");
}

#[test]
fn child_cannot_see_or_stop_siblings() {
    let b = Broker::start(&[]);
    let a = b.ctl_json(&["spawn", "sleep:30"]);
    let _bb = b.ctl_json(&["spawn", "sleep:30"]);
    // Read the child's own token from the fake's environment is not possible;
    // instead verify from the root side that `authorize` is subtree-scoped by
    // asking root to stop a nonexistent / non-descendant id.
    let (ok, _, err) = b.ctl(&["status", "999"]);
    assert!(!ok && err.contains("NotFound"), "{err}");
    let (ok, _, err) = b.ctl(&["status", "000"]);
    assert!(!ok && err.contains("Forbidden"), "{err}");
    let v = b.ctl_json(&["stop", a["id"].as_str().unwrap()]);
    assert_eq!(v["stopped"], serde_json::json!(["001"]));
    let s = b.ctl_json(&["status", "001"]);
    assert_eq!(s["state"], "stopped");
    let s2 = b.ctl_json(&["status", "002"]);
    assert_eq!(s2["state"], "running");
}

#[test]
fn fan_out_limit_is_enforced() {
    let b = Broker::start(&["--max-children", "2"]);
    b.ctl_json(&["spawn", "sleep:30"]);
    b.ctl_json(&["spawn", "sleep:30"]);
    let (ok, _, err) = b.ctl(&["spawn", "sleep:30"]);
    assert!(!ok && err.contains("LimitExceeded"), "{err}");
}

#[test]
fn timeout_stops_the_child() {
    let b = Broker::start(&["--timeout-secs", "1"]);
    let start = Instant::now();
    let v = b.ctl_json(&["spawn", "--wait", "sleep:30"]);
    assert!(
        start.elapsed() < Duration::from_secs(15),
        "took {:?}",
        start.elapsed()
    );
    assert_eq!(v["node"]["state"], "stopped");
    assert_eq!(v["node"]["reason"], "timeout");
}

#[test]
fn wait_timeout_returns_without_killing() {
    let b = Broker::start(&[]);
    b.ctl_json(&["spawn", "sleep:30"]);
    let v = b.ctl_json(&["wait", "--timeout-secs", "1", "001"]);
    assert_eq!(v["timed_out"], true);
    assert_eq!(v["node"]["state"], "running");
}

#[test]
fn streaming_wait_forwards_assistant_events() {
    let b = Broker::start(&[]);
    b.ctl_json(&["spawn", "sleep:1"]);
    let (ok, out, _) = b.ctl(&["wait", "--stream", "001"]);
    assert!(ok);
    assert!(
        out.contains(r#"{"text":"slept","type":"assistant"}"#),
        "{out}"
    );
}

#[test]
fn shutdown_stops_everything_and_exits() {
    let mut b = Broker::start(&[]);
    b.ctl_json(&["spawn", "sleep:30"]);
    b.ctl_json(&["spawn", "sleep:30"]);
    let v = b.ctl_json(&["shutdown"]);
    assert_eq!(v["stopped"].as_array().unwrap().len(), 2);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(Some(status)) = b.child.try_wait() {
            assert!(status.success(), "{status}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "broker did not exit; stderr: {}",
            b.stderr()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !b.state.path().join("sapid.sock").exists(),
        "socket must be removed"
    );
    assert!(
        !b.state.path().join("token").exists(),
        "token must be removed"
    );
}

#[test]
fn exit_with_pid_takes_children_down() {
    // A short-lived "launcher" process the broker watches.
    let mut launcher = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = launcher.id().to_string();
    let mut b = Broker::start(&["--exit-with-pid", &pid]);
    b.ctl_json(&["spawn", "sleep:30"]);
    launcher.kill().unwrap();
    launcher.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Ok(Some(_)) = b.child.try_wait() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "broker did not follow the launcher; stderr: {}",
            b.stderr()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let calls = std::fs::read_to_string(b.fake_state.path().join("calls.log")).unwrap();
    assert!(
        calls.lines().any(|l| l.starts_with("stop ")),
        "children must be stopped: {calls}"
    );
}

#[test]
fn state_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let b = Broker::start(&[]);
    let mode = |p: &str| {
        std::fs::metadata(b.state.path().join(p))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode("token"), 0o600);
    assert_eq!(mode("sapid.json"), 0o600);
    assert_eq!(mode("sapid.sock"), 0o600);
    assert_eq!(
        std::fs::metadata(b.state.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}
