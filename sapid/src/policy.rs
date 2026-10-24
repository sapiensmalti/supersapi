//! Capability attenuation.
//!
//! Every node runs under a [`Profile`]. A child's profile is derived from its
//! parent's and the spawn parameters by [`derive_child`], which enforces the
//! single rule that makes the swarm safe to reason about:
//!
//! > A child can never do more than its parent.
//!
//! Concretely: the mounted directory must lie inside the parent's directory
//! (after resolving symlinks), access and network can only narrow, time and
//! resource budgets can only shrink, and depth, fan-out and total population are
//! bounded by the broker's [`Limits`]. There is no override. This module has no
//! I/O other than path canonicalisation and no knowledge of Docker.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::protocol::{Access, Limits, Network, SpawnParams};

/// What a node may do.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    /// Absolute, canonical host directory this node sees as `/workspace`.
    /// Meaningless when `access == Access::None`.
    pub host_dir: PathBuf,
    /// Filesystem access.
    pub access: Access,
    /// Network reach.
    pub network: Network,
    /// Wall-clock budget in seconds.
    pub timeout_secs: u64,
    /// CPU quota.
    pub cpus: f64,
    /// Memory limit in MiB.
    pub memory_mib: u64,
    /// Depth below the root.
    pub depth: u32,
}

/// Why a spawn was refused.
#[derive(Debug, Error, PartialEq)]
pub enum PolicyError {
    /// Requested directory is outside the parent's.
    #[error("directory escapes the caller's workspace: {0}")]
    DirEscapes(String),
    /// Directory does not exist or is not a directory.
    #[error("not a directory: {0}")]
    NotADirectory(String),
    /// Child asked for wider access than the parent has.
    #[error("access {requested:?} is wider than the caller's {allowed:?}")]
    AccessWidens {
        /// What was asked for.
        requested: Access,
        /// What the caller has.
        allowed: Access,
    },
    /// Child asked for wider network than the parent has.
    #[error("network {requested:?} is wider than the caller's {allowed:?}")]
    NetworkWidens {
        /// What was asked for.
        requested: Network,
        /// What the caller has.
        allowed: Network,
    },
    /// Child asked for a larger budget than the parent has.
    #[error("{what} {requested} exceeds the caller's {allowed}")]
    BudgetExceeds {
        /// Which budget.
        what: &'static str,
        /// What was asked for.
        requested: String,
        /// What the caller has.
        allowed: String,
    },
    /// Tree would become too deep.
    #[error("maximum depth {0} reached")]
    TooDeep(u32),
    /// Caller has too many running children.
    #[error("caller already has {0} running children")]
    TooManyChildren(u32),
    /// Swarm has too many running nodes.
    #[error("swarm already has {0} running nodes")]
    TooManyTotal(u32),
    /// Zero or absurd values.
    #[error("invalid value for {0}")]
    Invalid(&'static str),
}

/// Live counts the policy needs to check limits.
#[derive(Debug, Clone, Copy)]
pub struct Population {
    /// Running children of the caller.
    pub running_children: u32,
    /// Running nodes in the whole swarm.
    pub running_total: u32,
}

/// Derive a child's profile from its parent's and the request.
pub fn derive_child(
    parent: &Profile,
    params: &SpawnParams,
    limits: Limits,
    population: Population,
) -> Result<Profile, PolicyError> {
    if parent.depth + 1 > limits.max_depth {
        return Err(PolicyError::TooDeep(limits.max_depth));
    }
    if population.running_children >= limits.max_children {
        return Err(PolicyError::TooManyChildren(population.running_children));
    }
    if population.running_total >= limits.max_total {
        return Err(PolicyError::TooManyTotal(population.running_total));
    }

    let access = params.access.unwrap_or(parent.access);
    if access > parent.access {
        return Err(PolicyError::AccessWidens {
            requested: access,
            allowed: parent.access,
        });
    }

    let network = params.network.unwrap_or(parent.network);
    if network > parent.network {
        return Err(PolicyError::NetworkWidens {
            requested: network,
            allowed: parent.network,
        });
    }

    let timeout_secs = params.timeout_secs.unwrap_or(parent.timeout_secs);
    if timeout_secs == 0 {
        return Err(PolicyError::Invalid("timeout_secs"));
    }
    if timeout_secs > parent.timeout_secs {
        return Err(PolicyError::BudgetExceeds {
            what: "timeout_secs",
            requested: timeout_secs.to_string(),
            allowed: parent.timeout_secs.to_string(),
        });
    }

    let cpus = params.cpus.unwrap_or(parent.cpus);
    if cpus <= 0.0 || !cpus.is_finite() {
        return Err(PolicyError::Invalid("cpus"));
    }
    if cpus > parent.cpus {
        return Err(PolicyError::BudgetExceeds {
            what: "cpus",
            requested: cpus.to_string(),
            allowed: parent.cpus.to_string(),
        });
    }

    let memory_mib = params.memory_mib.unwrap_or(parent.memory_mib);
    if memory_mib < 64 {
        return Err(PolicyError::Invalid("memory_mib"));
    }
    if memory_mib > parent.memory_mib {
        return Err(PolicyError::BudgetExceeds {
            what: "memory_mib",
            requested: memory_mib.to_string(),
            allowed: parent.memory_mib.to_string(),
        });
    }

    let host_dir = if access == Access::None {
        parent.host_dir.clone()
    } else {
        resolve_within(&parent.host_dir, params.dir.as_deref().unwrap_or("."))?
    };

    Ok(Profile {
        host_dir,
        access,
        network,
        timeout_secs,
        cpus,
        memory_mib,
        depth: parent.depth + 1,
    })
}

/// Resolve `rel` against `base` and prove the result is inside `base`.
///
/// Both sides are canonicalised so symlinks pointing outside the workspace
/// are caught. Absolute paths are rejected outright.
pub fn resolve_within(base: &Path, rel: &str) -> Result<PathBuf, PolicyError> {
    if rel.is_empty() {
        return Err(PolicyError::Invalid("dir"));
    }
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(PolicyError::DirEscapes(rel.to_owned()));
    }
    let base = base
        .canonicalize()
        .map_err(|_| PolicyError::NotADirectory(base.display().to_string()))?;
    let joined = base.join(rel_path);
    let resolved = joined
        .canonicalize()
        .map_err(|_| PolicyError::NotADirectory(rel.to_owned()))?;
    if !resolved.is_dir() {
        return Err(PolicyError::NotADirectory(rel.to_owned()));
    }
    if !resolved.starts_with(&base) {
        return Err(PolicyError::DirEscapes(rel.to_owned()));
    }
    Ok(resolved)
}

/// Express `dir` relative to `root` for display. Falls back to `.` when
/// `dir` is `root` itself.
pub fn display_relative(root: &Path, dir: &Path) -> String {
    match dir.strip_prefix(root) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".to_owned(),
        Ok(rel) => rel.display().to_string(),
        Err(_) => dir.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn limits() -> Limits {
        Limits {
            max_children: 4,
            max_depth: 3,
            max_total: 16,
        }
    }

    fn pop() -> Population {
        Population {
            running_children: 0,
            running_total: 1,
        }
    }

    fn root(dir: &Path) -> Profile {
        Profile {
            host_dir: dir.canonicalize().unwrap(),
            access: Access::ReadWrite,
            network: Network::Full,
            timeout_secs: 3600,
            cpus: 4.0,
            memory_mib: 4096,
            depth: 0,
        }
    }

    fn params(task: &str) -> SpawnParams {
        SpawnParams {
            task: task.to_owned(),
            ..SpawnParams::default()
        }
    }

    #[test]
    fn child_inherits_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = root(tmp.path());
        let child = derive_child(&parent, &params("x"), limits(), pop()).unwrap();
        assert_eq!(child.host_dir, parent.host_dir);
        assert_eq!(child.access, Access::ReadWrite);
        assert_eq!(child.network, Network::Full);
        assert_eq!(child.timeout_secs, 3600);
        assert_eq!(child.depth, 1);
    }

    #[test]
    fn subdirectory_is_allowed_and_canonical() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("a/b")).unwrap();
        let parent = root(tmp.path());
        let mut p = params("x");
        p.dir = Some("a/./b/../b".to_owned());
        let child = derive_child(&parent, &p, limits(), pop()).unwrap();
        assert_eq!(child.host_dir, parent.host_dir.join("a/b"));
    }

    #[test]
    fn dotdot_escape_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = root(tmp.path());
        let mut p = params("x");
        p.dir = Some("..".to_owned());
        assert!(matches!(
            derive_child(&parent, &p, limits(), pop()),
            Err(PolicyError::DirEscapes(_))
        ));
    }

    #[test]
    fn absolute_path_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = root(tmp.path());
        let mut p = params("x");
        p.dir = Some("/etc".to_owned());
        assert!(matches!(
            derive_child(&parent, &p, limits(), pop()),
            Err(PolicyError::DirEscapes(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("link")).unwrap();
        let parent = root(tmp.path());
        let mut p = params("x");
        p.dir = Some("link".to_owned());
        assert!(matches!(
            derive_child(&parent, &p, limits(), pop()),
            Err(PolicyError::DirEscapes(_))
        ));
    }

    #[test]
    fn missing_dir_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = root(tmp.path());
        let mut p = params("x");
        p.dir = Some("nope".to_owned());
        assert!(matches!(
            derive_child(&parent, &p, limits(), pop()),
            Err(PolicyError::NotADirectory(_))
        ));
    }

    #[test]
    fn access_can_only_narrow() {
        let tmp = tempfile::tempdir().unwrap();
        let mut parent = root(tmp.path());
        parent.access = Access::ReadOnly;
        let mut p = params("x");
        p.access = Some(Access::ReadWrite);
        assert!(matches!(
            derive_child(&parent, &p, limits(), pop()),
            Err(PolicyError::AccessWidens { .. })
        ));
        p.access = Some(Access::None);
        let child = derive_child(&parent, &p, limits(), pop()).unwrap();
        assert_eq!(child.access, Access::None);
    }

    #[test]
    fn scratch_child_ignores_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = root(tmp.path());
        let mut p = params("x");
        p.access = Some(Access::None);
        p.dir = Some("../../etc".to_owned());
        let child = derive_child(&parent, &p, limits(), pop()).unwrap();
        assert_eq!(child.access, Access::None);
    }

    #[test]
    fn network_can_only_narrow() {
        let tmp = tempfile::tempdir().unwrap();
        let mut parent = root(tmp.path());
        parent.network = Network::None;
        let mut p = params("x");
        p.network = Some(Network::Full);
        assert!(matches!(
            derive_child(&parent, &p, limits(), pop()),
            Err(PolicyError::NetworkWidens { .. })
        ));
    }

    #[test]
    fn budgets_can_only_shrink() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = root(tmp.path());
        for (field, p) in [
            (
                "timeout_secs",
                SpawnParams {
                    timeout_secs: Some(7200),
                    ..params("x")
                },
            ),
            (
                "cpus",
                SpawnParams {
                    cpus: Some(8.0),
                    ..params("x")
                },
            ),
            (
                "memory_mib",
                SpawnParams {
                    memory_mib: Some(8192),
                    ..params("x")
                },
            ),
        ] {
            match derive_child(&parent, &p, limits(), pop()) {
                Err(PolicyError::BudgetExceeds { what, .. }) => assert_eq!(what, field),
                other => panic!("{field}: {other:?}"),
            }
        }
        let p = SpawnParams {
            timeout_secs: Some(60),
            cpus: Some(0.5),
            memory_mib: Some(512),
            ..params("x")
        };
        let child = derive_child(&parent, &p, limits(), pop()).unwrap();
        assert_eq!((child.timeout_secs, child.memory_mib), (60, 512));
        assert!((child.cpus - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn nonsense_values_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = root(tmp.path());
        for p in [
            SpawnParams {
                timeout_secs: Some(0),
                ..params("x")
            },
            SpawnParams {
                cpus: Some(0.0),
                ..params("x")
            },
            SpawnParams {
                cpus: Some(f64::NAN),
                ..params("x")
            },
            SpawnParams {
                memory_mib: Some(1),
                ..params("x")
            },
            SpawnParams {
                dir: Some(String::new()),
                ..params("x")
            },
        ] {
            assert!(
                matches!(
                    derive_child(&parent, &p, limits(), pop()),
                    Err(PolicyError::Invalid(_))
                ),
                "{p:?}"
            );
        }
    }

    #[test]
    fn limits_are_enforced() {
        let tmp = tempfile::tempdir().unwrap();
        let mut parent = root(tmp.path());
        let l = limits();
        assert!(matches!(
            derive_child(
                &parent,
                &params("x"),
                l,
                Population {
                    running_children: 4,
                    running_total: 5
                }
            ),
            Err(PolicyError::TooManyChildren(4))
        ));
        assert!(matches!(
            derive_child(
                &parent,
                &params("x"),
                l,
                Population {
                    running_children: 0,
                    running_total: 16
                }
            ),
            Err(PolicyError::TooManyTotal(16))
        ));
        parent.depth = 3;
        assert!(matches!(
            derive_child(&parent, &params("x"), l, pop()),
            Err(PolicyError::TooDeep(3))
        ));
    }

    #[test]
    fn display_relative_handles_root_and_children() {
        let root = Path::new("/w");
        assert_eq!(display_relative(root, Path::new("/w")), ".");
        assert_eq!(display_relative(root, Path::new("/w/a/b")), "a/b");
    }
}
