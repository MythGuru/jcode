use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const PEER_GROUPS_VERSION: u32 = 1;
const INVALID_PREFIX: &str = "Peer groups configuration is invalid:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerGroups {
    groups: Vec<PeerGroup>,
    load_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerGroup {
    pub name: String,
    pub members: Vec<PeerMember>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerMember {
    pub alias: String,
    pub working_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
struct RawPeerGroups {
    version: u32,
    groups: Vec<RawPeerGroup>,
}

#[derive(Debug, Deserialize)]
struct RawPeerGroup {
    name: String,
    members: Vec<RawPeerMember>,
}

#[derive(Debug, Deserialize)]
struct RawPeerMember {
    alias: String,
    working_dir: PathBuf,
}

impl PeerGroups {
    pub fn empty() -> Self {
        Self {
            groups: Vec::new(),
            load_error: None,
        }
    }

    pub fn invalid(error: impl Into<String>) -> Self {
        Self {
            groups: Vec::new(),
            load_error: Some(error.into()),
        }
    }

    pub fn load_from_jcode_home(jcode_home: &Path) -> Result<Self> {
        let path = jcode_home.join("peer-groups.json");
        if !path.exists() {
            return Ok(Self::empty());
        }

        let bytes = std::fs::read(&path)
            .with_context(|| format!("{INVALID_PREFIX} could not read {}", path.display()))?;
        let raw: RawPeerGroups = serde_json::from_slice(&bytes)
            .with_context(|| format!("{INVALID_PREFIX} malformed JSON"))?;
        Self::validate(raw).map_err(|error| anyhow::anyhow!("{INVALID_PREFIX} {error}"))
    }

    pub fn load_default() -> Result<Self> {
        let config_path = crate::config::Config::path()
            .ok_or_else(|| anyhow::anyhow!("{INVALID_PREFIX} no jcode home directory"))?;
        let home = config_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("{INVALID_PREFIX} invalid jcode home directory"))?;
        Self::load_from_jcode_home(home)
    }

    pub fn groups(&self) -> &[PeerGroup] {
        &self.groups
    }

    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    pub fn identity_for_dir(&self, working_dir: &Path) -> Option<(&PeerGroup, &PeerMember)> {
        let canonical = canonicalize_peer_dir(working_dir).ok()?;
        self.groups.iter().find_map(|group| {
            group
                .members
                .iter()
                .find(|member| path_key(&member.working_dir) == path_key(&canonical))
                .map(|member| (group, member))
        })
    }

    fn validate(raw: RawPeerGroups) -> Result<Self> {
        if raw.version != PEER_GROUPS_VERSION {
            bail!("unsupported version {}", raw.version);
        }

        let mut group_names = HashSet::new();
        let mut all_dirs = HashSet::new();
        let mut groups = Vec::with_capacity(raw.groups.len());

        for raw_group in raw.groups {
            let name = raw_group.name.trim().to_string();
            if name.is_empty() {
                bail!("group name must not be empty");
            }
            if !group_names.insert(name.clone()) {
                bail!("duplicate group name `{name}`");
            }
            if raw_group.members.len() < 2 {
                bail!("group `{name}` must contain at least two members");
            }

            let mut aliases = HashSet::new();
            let mut members = Vec::with_capacity(raw_group.members.len());
            for raw_member in raw_group.members {
                let alias = raw_member.alias.trim().to_string();
                if alias.is_empty() {
                    bail!("alias in group `{name}` must not be empty");
                }
                if !aliases.insert(alias.to_lowercase()) {
                    bail!("duplicate alias `{alias}` in group `{name}`");
                }
                if !raw_member.working_dir.is_absolute() {
                    bail!("working directory for `{alias}` must be absolute");
                }
                let working_dir =
                    canonicalize_peer_dir(&raw_member.working_dir).with_context(|| {
                        format!(
                            "working directory for `{alias}` cannot be canonicalized: {}",
                            raw_member.working_dir.display()
                        )
                    })?;
                if !all_dirs.insert(path_key(&working_dir)) {
                    bail!(
                        "working directory is configured more than once: {}",
                        working_dir.display()
                    );
                }
                members.push(PeerMember { alias, working_dir });
            }
            groups.push(PeerGroup { name, members });
        }

        Ok(Self {
            groups,
            load_error: None,
        })
    }
}

fn canonicalize_peer_dir(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path).with_context(|| format!("cannot canonicalize {}", path.display()))
}

fn path_key(path: &Path) -> String {
    let value = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        value.to_lowercase()
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_snapshot_preserves_startup_error_without_members() {
        let groups = PeerGroups::invalid("Peer groups configuration is invalid: malformed JSON");

        assert!(groups.groups().is_empty());
        assert_eq!(
            groups.load_error(),
            Some("Peer groups configuration is invalid: malformed JSON")
        );
    }
}
