//! Container runtime: the only module that knows about Docker.
//!
//! Containers are created with the `docker` CLI, never the Docker API: the
//! argument vector built by [`ChildSpec::argv`] is the complete, auditable
//! description of what a child may touch. The binary path is configurable so
//! tests can substitute a fake.

use std::path::PathBuf;
use std::process::Stdio;

use tokio::process::{Child, Command};

use crate::policy::Profile;
use crate::protocol::{Access, Network};

/// Capabilities kept after `--cap-drop ALL`. Exactly what the image's
/// entrypoint needs to repair volume ownership and drop to the host uid.
pub const CAPABILITIES: &[&str] = &[
    "CHOWN",
    "DAC_OVERRIDE",
    "DAC_READ_SEARCH",
    "FOWNER",
    "SETUID",
    "SETGID",
    "SETPCAP",
];

/// Where the broker socket appears inside every container.
pub const CONTAINER_SOCKET: &str = "/run/sapid/sapid.sock";

/// Path of the swarm extension inside the image.
pub const EXTENSION_PATH: &str = "/opt/supersapi/extension/index.ts";

/// How containers reach the broker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// A Unix socket bind-mounted into the container.
    Unix(PathBuf),
    /// TCP; the container connects to `host.docker.internal:port`.
    Tcp {
        /// Host port the broker listens on.
        port: u16,
        /// Add `host.docker.internal` via `host-gateway` (needed on Linux).
        add_host: bool,
    },
}

impl Transport {
    /// Address string the extension reads from `SAPID_ADDR`.
    pub fn address_for_container(&self) -> String {
        match self {
            Transport::Unix(_) => format!("unix://{CONTAINER_SOCKET}"),
            Transport::Tcp { port, .. } => format!("tcp://host.docker.internal:{port}"),
        }
    }
}

/// Everything needed to run one child.
#[derive(Debug, Clone)]
pub struct ChildSpec {
    /// Container name.
    pub name: String,
    /// Image tag.
    pub image: String,
    /// Swarm id (label).
    pub swarm: String,
    /// Node id (label).
    pub node: String,
    /// Parent node id (label).
    pub parent: String,
    /// The child's profile.
    pub profile: Profile,
    /// Docker volume holding pi's credentials and settings.
    pub config_volume: String,
    /// Broker transport.
    pub transport: Transport,
    /// uid:gid to run as (Linux); `None` means root.
    pub user: Option<(u32, u32)>,
    /// Model passed to pi, if any.
    pub model: Option<String>,
    /// Trust `.pi/` files in the workspace.
    pub approve_project: bool,
    /// The task prompt.
    pub task: String,
    /// Extra arguments inserted before the image (for tests and operators).
    pub extra_args: Vec<String>,
}

impl ChildSpec {
    /// The complete `docker` argument vector for this child.
    ///
    /// The token is **not** here: it travels through the environment of the
    /// `docker` process (`-e SAPID_TOKEN` with no value), so it never appears
    /// in a process listing.
    #[allow(clippy::too_many_lines)]
    pub fn argv(&self) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "run".into(),
            "--rm".into(),
            "--init".into(),
            "--name".into(),
            self.name.clone(),
            "--hostname".into(),
            self.node.clone(),
            "--label".into(),
            "sh.supersapi.managed=true".into(),
            "--label".into(),
            format!("sh.supersapi.swarm={}", self.swarm),
            "--label".into(),
            format!("sh.supersapi.node={}", self.node),
            "--label".into(),
            format!("sh.supersapi.parent={}", self.parent),
            "--workdir".into(),
            "/workspace".into(),
        ];

        match self.profile.access {
            Access::ReadWrite => {
                a.push("--volume".into());
                a.push(format!("{}:/workspace", self.profile.host_dir.display()));
            }
            Access::ReadOnly => {
                a.push("--volume".into());
                a.push(format!("{}:/workspace:ro", self.profile.host_dir.display()));
            }
            Access::None => {
                // Anonymous volume, removed with the container.
                a.push("--volume".into());
                a.push("/workspace".into());
            }
        }

        a.push("--mount".into());
        a.push(format!(
            "type=volume,source={},target=/pi-config",
            self.config_volume
        ));

        match &self.transport {
            Transport::Unix(host_sock) => {
                a.push("--volume".into());
                a.push(format!("{}:{CONTAINER_SOCKET}", host_sock.display()));
            }
            Transport::Tcp { add_host, .. } => {
                if *add_host {
                    a.push("--add-host".into());
                    a.push("host.docker.internal:host-gateway".into());
                }
            }
        }

        if self.profile.network == Network::None {
            a.push("--network".into());
            a.push("none".into());
        }

        a.push("--cpus".into());
        a.push(format!("{}", self.profile.cpus));
        a.push("--memory".into());
        a.push(format!("{}m", self.profile.memory_mib));
        a.push("--pids-limit".into());
        a.push("1024".into());

        a.push("--security-opt".into());
        a.push("no-new-privileges".into());
        a.push("--cap-drop".into());
        a.push("ALL".into());
        for cap in CAPABILITIES {
            a.push("--cap-add".into());
            a.push((*cap).into());
        }

        for (k, v) in [
            ("PI_CODING_AGENT_DIR", "/pi-config".to_owned()),
            ("PI_CODING_AGENT_SESSION_DIR", "/tmp/pi-sessions".to_owned()),
            ("GIT_CONFIG_GLOBAL", "/pi-config/gitconfig".to_owned()),
            ("SAPI_AUTO_UPDATE", "0".to_owned()),
            ("SAPID_ADDR", self.transport.address_for_container()),
            ("SAPID_NODE", self.node.clone()),
            ("SAPID_DEPTH", self.profile.depth.to_string()),
        ] {
            a.push("--env".into());
            a.push(format!("{k}={v}"));
        }
        // Value deliberately omitted: inherited from the docker process env.
        a.push("--env".into());
        a.push("SAPID_TOKEN".into());

        if let Some((uid, gid)) = self.user {
            a.push("--env".into());
            a.push(format!("SAPI_UID={uid}"));
            a.push("--env".into());
            a.push(format!("SAPI_GID={gid}"));
        }

        a.extend(self.extra_args.iter().cloned());

        a.push(self.image.clone());

        // Arguments to the image entrypoint, i.e. to `pi`.
        a.push("--mode".into());
        a.push("json".into());
        a.push("-p".into());
        a.push("--no-session".into());
        a.push(
            if self.approve_project {
                "--approve"
            } else {
                "--no-approve"
            }
            .into(),
        );
        a.push("--extension".into());
        a.push(EXTENSION_PATH.into());
        if let Some(model) = &self.model {
            a.push("--model".into());
            a.push(model.clone());
        }
        a.push("--".into());
        a.push(self.task.clone());
        a
    }
}

/// Handle on the `docker` binary.
#[derive(Debug, Clone)]
pub struct Docker {
    bin: PathBuf,
}

impl Docker {
    /// Use the given binary (normally just `docker`).
    pub fn new(bin: impl Into<PathBuf>) -> Self {
        Self { bin: bin.into() }
    }

    /// Start a child container. Stdout and stderr are piped.
    pub fn run(&self, spec: &ChildSpec, token: &str) -> std::io::Result<Child> {
        Command::new(&self.bin)
            .args(spec.argv())
            .env("SAPID_TOKEN", token)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
    }

    /// Stop a container by name, waiting up to `grace` seconds before SIGKILL.
    pub async fn stop(&self, name: &str, grace: u32) -> std::io::Result<bool> {
        let status = Command::new(&self.bin)
            .args(["stop", "--time", &grace.to_string(), name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?;
        Ok(status.success())
    }

    /// `docker info` succeeds.
    pub async fn is_available(&self) -> bool {
        Command::new(&self.bin)
            .args(["info"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok_and(|s| s.success())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(access: Access, network: Network) -> ChildSpec {
        ChildSpec {
            name: "ssapi-abc-001".into(),
            image: "supersapi:deadbeef".into(),
            swarm: "abc".into(),
            node: "001".into(),
            parent: "000".into(),
            profile: Profile {
                host_dir: PathBuf::from("/home/u/proj/sub"),
                access,
                network,
                timeout_secs: 60,
                cpus: 1.5,
                memory_mib: 1024,
                depth: 1,
            },
            config_volume: "sapi-config".into(),
            transport: Transport::Unix(PathBuf::from(
                "/home/u/.local/state/supersapi/abc/sapid.sock",
            )),
            user: Some((1000, 1000)),
            model: Some("m".into()),
            approve_project: false,
            task: "do the thing".into(),
            extra_args: vec![],
        }
    }

    fn joined(a: &[String]) -> String {
        a.join(" ")
    }

    #[test]
    fn read_write_mounts_dir_rw() {
        let s = joined(&spec(Access::ReadWrite, Network::Full).argv());
        assert!(s.contains("--volume /home/u/proj/sub:/workspace "), "{s}");
        assert!(!s.contains(":ro"), "{s}");
        assert!(!s.contains("--network none"), "{s}");
    }

    #[test]
    fn read_only_mounts_dir_ro() {
        let s = joined(&spec(Access::ReadOnly, Network::Full).argv());
        assert!(s.contains("--volume /home/u/proj/sub:/workspace:ro"), "{s}");
    }

    #[test]
    fn scratch_mounts_anonymous_volume_and_never_the_dir() {
        let s = joined(&spec(Access::None, Network::None).argv());
        assert!(s.contains("--volume /workspace "), "{s}");
        assert!(!s.contains("/home/u/proj"), "{s}");
        assert!(s.contains("--network none"), "{s}");
    }

    #[test]
    fn hardening_is_always_present() {
        let a = spec(Access::ReadWrite, Network::Full).argv();
        let s = joined(&a);
        assert!(s.contains("--rm --init"));
        assert!(s.contains("--security-opt no-new-privileges"));
        assert!(s.contains("--cap-drop ALL"));
        for cap in CAPABILITIES {
            assert!(s.contains(&format!("--cap-add {cap}")), "{cap}");
        }
        assert!(s.contains("--cpus 1.5 --memory 1024m --pids-limit 1024"));
        assert!(
            !s.contains("docker.sock"),
            "the docker socket must never be mounted"
        );
        assert!(!s.contains("--privileged"));
    }

    #[test]
    fn token_is_passed_by_name_only() {
        let a = spec(Access::ReadWrite, Network::Full).argv();
        let pos = a.iter().position(|x| x == "SAPID_TOKEN").unwrap();
        assert_eq!(a[pos - 1], "--env");
        assert!(!a.iter().any(|x| x.starts_with("SAPID_TOKEN=")));
    }

    #[test]
    fn pi_arguments_come_after_image() {
        let a = spec(Access::ReadWrite, Network::Full).argv();
        let img = a.iter().position(|x| x == "supersapi:deadbeef").unwrap();
        let tail = joined(&a[img + 1..]);
        assert_eq!(
            tail,
            "--mode json -p --no-session --no-approve --extension /opt/supersapi/extension/index.ts --model m -- do the thing"
        );
    }

    #[test]
    fn tcp_transport_sets_address_and_optional_host_gateway() {
        let mut s = spec(Access::ReadWrite, Network::Full);
        s.transport = Transport::Tcp {
            port: 4242,
            add_host: true,
        };
        let j = joined(&s.argv());
        assert!(j.contains("--add-host host.docker.internal:host-gateway"));
        assert!(j.contains("SAPID_ADDR=tcp://host.docker.internal:4242"));
        assert!(!j.contains("/run/sapid/sapid.sock"));
        s.transport = Transport::Tcp {
            port: 4242,
            add_host: false,
        };
        assert!(!joined(&s.argv()).contains("--add-host"));
    }

    #[test]
    fn uid_mapping_is_optional() {
        let mut s = spec(Access::ReadWrite, Network::Full);
        assert!(joined(&s.argv()).contains("SAPI_UID=1000"));
        s.user = None;
        assert!(!joined(&s.argv()).contains("SAPI_UID"));
    }
}
