//! Keep git's repository discovery from walking into a home-directory repo.
//!
//! Git treats "no `.git` here" as "look in my parent, and its parent, and so
//! on". That is normally harmless. It is catastrophic when the *home directory
//! itself* is a repository, because any git command Jcode runs from a directory
//! that is not inside a real repo (a scratch folder, a freshly created project,
//! a temp dir) then silently adopts the home repo and walks everything the user
//! owns. Measured on such a machine: `git status --untracked-files=all` from an
//! empty scratch dir under home was still running after 45 seconds. Worse,
//! `source_state` hashes the *contents* of every untracked file it is handed.
//!
//! To the user that is indistinguishable from Jcode freezing: no error, no
//! crash, just a subprocess quietly enumerating their home directory.
//!
//! `GIT_CEILING_DIRECTORIES` tells git where to stop climbing, and setting it
//! once at startup covers every git call in the process, including those in
//! other crates, with no need to find and guard each site.
//!
//! This is deliberately conservative. The ceiling is installed *only* when the
//! home directory is itself a repo, because a ceiling is not free: it also stops
//! discovery for plain subdirectories that legitimately belong to that home
//! repo. On the overwhelmingly common setup, where home is not a repo, nothing
//! is set and behaviour is completely unchanged.

use std::path::{Path, PathBuf};

/// Stop git's upward repo search at the home directory, but only when home is
/// itself a repository and would therefore swallow unrelated directories.
///
/// Idempotent, and never overrides a value the user set themselves.
pub fn install_git_discovery_ceiling() {
    if std::env::var_os("GIT_CEILING_DIRECTORIES").is_some() {
        return;
    }

    let Some(home) = home_dir() else {
        return;
    };

    // Git ignores relative ceiling entries.
    if !home.is_absolute() || !home_is_a_repo(&home) {
        return;
    }

    unsafe {
        std::env::set_var("GIT_CEILING_DIRECTORIES", home.as_os_str());
    }
}

/// A repo has `.git` as a directory, or as a file for worktrees and submodules.
fn home_is_a_repo(home: &Path) -> bool {
    home.join(".git").exists()
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            return Some(PathBuf::from(profile));
        }
    }
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CeilingEnv {
        previous_ceiling: Option<std::ffi::OsString>,
        previous_home: Option<std::ffi::OsString>,
        key: &'static str,
    }

    impl CeilingEnv {
        fn set_home(home: &Path) -> Self {
            let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
            let guard = Self {
                previous_ceiling: std::env::var_os("GIT_CEILING_DIRECTORIES"),
                previous_home: std::env::var_os(key),
                key,
            };
            unsafe {
                std::env::remove_var("GIT_CEILING_DIRECTORIES");
                std::env::set_var(key, home.as_os_str());
            }
            guard
        }
    }

    impl Drop for CeilingEnv {
        fn drop(&mut self) {
            unsafe {
                match self.previous_ceiling.take() {
                    Some(value) => std::env::set_var("GIT_CEILING_DIRECTORIES", value),
                    None => std::env::remove_var("GIT_CEILING_DIRECTORIES"),
                }
                match self.previous_home.take() {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn a_home_that_is_a_repo_gets_a_ceiling() {
        let _lock = crate::storage::lock_test_env();
        let home = tempfile::tempdir().expect("temp home");
        std::fs::create_dir_all(home.path().join(".git")).expect("home .git");
        let _env = CeilingEnv::set_home(home.path());

        install_git_discovery_ceiling();

        let installed = std::env::var_os("GIT_CEILING_DIRECTORIES")
            .expect("a home-directory repo must be fenced off");
        assert_eq!(Path::new(&installed), home.path());
    }

    /// The common case. Setting a ceiling here would be pure downside, since it
    /// also blocks discovery for legitimate subdirectories of a home repo.
    #[test]
    fn an_ordinary_home_is_left_alone() {
        let _lock = crate::storage::lock_test_env();
        let home = tempfile::tempdir().expect("temp home");
        let _env = CeilingEnv::set_home(home.path());

        install_git_discovery_ceiling();

        assert!(
            std::env::var_os("GIT_CEILING_DIRECTORIES").is_none(),
            "no ceiling should be installed when home is not a repo"
        );
    }

    #[test]
    fn an_explicit_user_ceiling_wins() {
        let _lock = crate::storage::lock_test_env();
        let home = tempfile::tempdir().expect("temp home");
        std::fs::create_dir_all(home.path().join(".git")).expect("home .git");
        let _env = CeilingEnv::set_home(home.path());
        unsafe {
            std::env::set_var("GIT_CEILING_DIRECTORIES", "/explicit/user/choice");
        }

        install_git_discovery_ceiling();

        assert_eq!(
            std::env::var("GIT_CEILING_DIRECTORIES").ok().as_deref(),
            Some("/explicit/user/choice")
        );
    }

    /// The unit tests above prove we set the variable. This proves the variable
    /// actually does the job: real git, a real home-directory repo, and a real
    /// scratch directory inside it that must NOT be swallowed.
    ///
    /// Without the ceiling this is the production bug: git climbs out of the
    /// scratch dir, adopts the home repo, and reports it as the toplevel.
    #[test]
    fn git_really_stops_at_the_ceiling() {
        let _lock = crate::storage::lock_test_env();

        let home = tempfile::tempdir().expect("temp home");
        let init = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(home.path())
            .status();
        // No usable git in this environment: nothing to assert about git's behaviour.
        if !matches!(init, Ok(status) if status.success()) {
            return;
        }

        let scratch = home.path().join("scratch-project");
        std::fs::create_dir_all(&scratch).expect("scratch dir");

        let toplevel_of = |ceiling: Option<&std::path::Path>| -> Option<String> {
            let mut cmd = std::process::Command::new("git");
            cmd.args(["rev-parse", "--show-toplevel"])
                .current_dir(&scratch);
            match ceiling {
                Some(dir) => {
                    cmd.env("GIT_CEILING_DIRECTORIES", dir);
                }
                None => {
                    cmd.env_remove("GIT_CEILING_DIRECTORIES");
                }
            }
            let out = cmd.output().ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        };

        // Baseline: the hazard is real in this environment.
        assert!(
            toplevel_of(None).is_some(),
            "without a ceiling git should climb out of the scratch dir into the home repo"
        );

        // With the ceiling git must refuse rather than adopt the home repo.
        assert_eq!(
            toplevel_of(Some(home.path())),
            None,
            "the ceiling must stop discovery before it reaches the home repo"
        );
    }

    /// Worktrees and submodules use a `.git` *file* rather than a directory.
    #[test]
    fn a_home_using_a_git_file_also_counts_as_a_repo() {
        let _lock = crate::storage::lock_test_env();
        let home = tempfile::tempdir().expect("temp home");
        std::fs::write(home.path().join(".git"), "gitdir: /elsewhere\n").expect("home .git file");
        let _env = CeilingEnv::set_home(home.path());

        install_git_discovery_ceiling();

        assert!(
            std::env::var_os("GIT_CEILING_DIRECTORIES").is_some(),
            "a .git file marks a repo just as a .git directory does"
        );
    }
}
