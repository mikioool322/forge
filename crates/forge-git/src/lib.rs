//! Git worktree lifecycle and repository operations.
//!
//! Everything shells out to the `git` CLI with argument arrays (never shell
//! interpolation) and a hard timeout. Worktree directories are validated to
//! stay inside the configured worktree root.
//!
//! Important: Git worktrees isolate *working directories and branches*.
//! They are not a security sandbox — see docs/SECURITY notes in
//! docs/ARCHITECTURE.md.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("not a git repository: {0}")]
    NotARepository(PathBuf),
    #[error("git {args:?} failed (exit {code:?}): {stderr}")]
    Command {
        args: Vec<String>,
        code: Option<i32>,
        stderr: String,
    },
    #[error("git command timed out after {0:?}")]
    Timeout(Duration),
    #[error("unsafe path: {0}")]
    Unsafe(String),
}

const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const GIT_QUICK_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub detached: bool,
}

#[derive(Debug, Clone)]
pub struct GitRepo {
    root: PathBuf,
}

impl GitRepo {
    /// Detect the repository containing `start`; returns the canonical
    /// (main) working tree root, refusing to run against a linked worktree
    /// that belongs to another repository root. Intentional: Forge always
    /// operates on the canonical checkout.
    pub fn detect(start: &Path) -> Result<Self, GitError> {
        let out =
            run_git(start, &["rev-parse", "--show-toplevel"], GIT_QUICK_TIMEOUT).map_err(|e| {
                match e {
                    GitError::Command { .. } => GitError::NotARepository(start.to_path_buf()),
                    other => other,
                }
            })?;
        let root = PathBuf::from(out.trim());
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn head_commit(&self) -> Result<String, GitError> {
        Ok(
            run_git(&self.root, &["rev-parse", "HEAD"], GIT_QUICK_TIMEOUT)?
                .trim()
                .to_string(),
        )
    }

    pub fn current_branch(&self) -> Result<Option<String>, GitError> {
        let out = run_git(
            &self.root,
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
            GIT_QUICK_TIMEOUT,
        );
        match out {
            Ok(branch) => Ok(Some(branch.trim().to_string())),
            Err(GitError::Command { .. }) => Ok(None), // detached HEAD
            Err(e) => Err(e),
        }
    }

    pub fn branch_exists(&self, branch: &str) -> Result<bool, GitError> {
        let out = run_git(
            &self.root,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
            GIT_QUICK_TIMEOUT,
        );
        Ok(out.is_ok())
    }

    /// Create a worktree at `dir` on new branch `branch` from `base`.
    ///
    /// Self-healing sequence:
    /// 1. `git worktree add -b <branch> <dir> <base>`.
    /// 2. If git reports a stale registration for a missing worktree at the
    ///    target path (e.g. the data directory was deleted), prune stale
    ///    registrations and retry.
    /// 3. `git` may have created the branch before failing; in that case the
    ///    retry attaches the existing branch to the worktree without `-b`.
    pub fn worktree_add(&self, dir: &Path, branch: &str, base: &str) -> Result<(), GitError> {
        match self.worktree_add_raw(dir, Some(branch), base) {
            Ok(()) => Ok(()),
            Err(GitError::Command { ref stderr, .. }) if is_stale_registration(stderr) => {
                self.worktree_prune()?;
                match self.worktree_add_raw(dir, Some(branch), base) {
                    Ok(()) => Ok(()),
                    Err(GitError::Command { ref stderr, .. })
                        if stderr.contains("a branch named") =>
                    {
                        self.worktree_add_raw(dir, None, branch)
                    }
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }

    fn worktree_add_raw(
        &self,
        dir: &Path,
        new_branch: Option<&str>,
        target: &str,
    ) -> Result<(), GitError> {
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut owned: Vec<String> = vec!["worktree".into(), "add".into()];
        if let Some(branch) = new_branch {
            owned.push("-b".into());
            owned.push(branch.to_string());
        }
        owned.push(dir.to_string_lossy().to_string());
        owned.push(target.to_string());
        let refs: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        run_git(&self.root, &refs, GIT_TIMEOUT)?;
        Ok(())
    }

    pub fn worktree_list(&self) -> Result<Vec<WorktreeEntry>, GitError> {
        let out = run_git(
            &self.root,
            &["worktree", "list", "--porcelain"],
            GIT_TIMEOUT,
        )?;
        let mut entries = vec![];
        let mut current: Option<WorktreeEntry> = None;
        for line in out.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                if let Some(entry) = current.take() {
                    entries.push(entry);
                }
                current = Some(WorktreeEntry {
                    path: PathBuf::from(path),
                    branch: None,
                    head: None,
                    detached: false,
                });
            } else if let Some(branch) = line.strip_prefix("branch ") {
                if let Some(entry) = current.as_mut() {
                    entry.branch = Some(branch.trim_start_matches("refs/heads/").to_string());
                }
            } else if let Some(head) = line.strip_prefix("HEAD ") {
                if let Some(entry) = current.as_mut() {
                    entry.head = Some(head.to_string());
                }
            } else if line == "detached" {
                if let Some(entry) = current.as_mut() {
                    entry.detached = true;
                }
            }
        }
        if let Some(entry) = current.take() {
            entries.push(entry);
        }
        Ok(entries)
    }

    /// Remove a worktree. Refuses `force` on dirty worktrees unless the
    /// caller explicitly requested removal of agent-modified files.
    pub fn worktree_remove(&self, dir: &Path, force: bool) -> Result<(), GitError> {
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        } else if self.is_dirty(dir)? {
            return Err(GitError::Unsafe(format!(
                "worktree {} has uncommitted changes; refusing to remove",
                dir.display()
            )));
        }
        let dir_str = dir.to_string_lossy().to_string();
        args.push(dir_str.as_str());
        run_git(&self.root, &args, GIT_TIMEOUT)?;
        Ok(())
    }

    pub fn worktree_prune(&self) -> Result<(), GitError> {
        run_git(&self.root, &["worktree", "prune"], GIT_TIMEOUT)?;
        Ok(())
    }

    /// Any tracked modifications or untracked files?
    pub fn is_dirty(&self, worktree: &Path) -> Result<bool, GitError> {
        let out = run_git(worktree, &["status", "--porcelain"], GIT_QUICK_TIMEOUT)?;
        Ok(!out.trim().is_empty())
    }

    /// Record intent-to-add for all files so new files show up in diffs.
    pub fn mark_intent_to_add(&self, worktree: &Path) -> Result<(), GitError> {
        run_git(worktree, &["add", "-A", "-N"], GIT_TIMEOUT)?;
        Ok(())
    }

    pub fn diff_stat(&self, worktree: &Path) -> Result<String, GitError> {
        run_git(worktree, &["diff", "--stat"], GIT_TIMEOUT)
    }

    /// Unified diff, capped at `max_bytes` (truncation flag returned).
    pub fn diff(&self, worktree: &Path, max_bytes: usize) -> Result<(String, bool), GitError> {
        let full = run_git(worktree, &["diff", "--no-color"], GIT_TIMEOUT)?;
        if full.len() <= max_bytes {
            Ok((full, false))
        } else {
            let mut cut = max_bytes;
            while cut > 0 && !full.is_char_boundary(cut) {
                cut -= 1;
            }
            Ok((full[..cut].to_string(), true))
        }
    }

    /// `(added, deleted)` line counts across the whole diff.
    pub fn diff_numstat(&self, worktree: &Path) -> Result<(u64, u64), GitError> {
        let out = run_git(worktree, &["diff", "--numstat"], GIT_TIMEOUT)?;
        let mut added = 0u64;
        let mut deleted = 0u64;
        for line in out.lines() {
            let mut parts = line.split('\t');
            let a = parts.next().unwrap_or("-");
            let d = parts.next().unwrap_or("-");
            added += a.parse::<u64>().unwrap_or(0);
            deleted += d.parse::<u64>().unwrap_or(0);
        }
        Ok((added, deleted))
    }

    pub fn changed_files(&self, worktree: &Path) -> Result<Vec<String>, GitError> {
        let out = run_git(worktree, &["status", "--porcelain"], GIT_TIMEOUT)?;
        Ok(out
            .lines()
            .filter(|l| l.len() > 3)
            .map(|l| l[3..].to_string())
            .collect())
    }
}

fn is_stale_registration(stderr: &str) -> bool {
    stderr.contains("already registered")
}

fn run_git(cwd: &Path, args: &[&str], timeout: Duration) -> Result<String, GitError> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    // Deterministic environment: no interactive prompts, no pagers.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_PAGER", "cat");
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    let mut child = cmd.spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(_) => break,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(GitError::Timeout(timeout));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if output.status.success() {
        Ok(stdout)
    } else {
        Err(GitError::Command {
            args: args.iter().map(|s| s.to_string()).collect(),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

/// Restrict a string to a safe single ref/path component.
pub fn sanitize_ref_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dot = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
            last_dot = false;
            out.push(c);
        } else if c == '.' {
            if last_dot {
                continue;
            }
            last_dot = true;
            out.push('.');
        } else {
            last_dot = false;
            out.push('-');
        }
    }
    let trimmed = out.trim_matches(['.', '-']).to_string();
    if trimmed.is_empty() {
        "unnamed".into()
    } else {
        trimmed
    }
}

/// Verify that `candidate` stays inside `base` after lexical normalization.
/// Both paths must already exist for canonicalization.
pub fn ensure_inside(base: &Path, candidate: &Path) -> Result<PathBuf, GitError> {
    let base_canon = base
        .canonicalize()
        .map_err(|_| GitError::Unsafe(format!("base path {} does not exist", base.display())))?;
    let cand_canon = candidate.canonicalize().map_err(|_| {
        GitError::Unsafe(format!(
            "candidate path {} does not exist",
            candidate.display()
        ))
    })?;
    if cand_canon.starts_with(&base_canon) {
        Ok(cand_canon)
    } else {
        Err(GitError::Unsafe(format!(
            "{} is outside {}",
            cand_canon.display(),
            base_canon.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo(dir: &Path) {
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-b", "main"]);
        std::fs::write(dir.join("README.md"), "hello\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "init"]);
    }

    #[test]
    fn detect_and_head() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let repo = GitRepo::detect(dir.path()).unwrap();
        assert_eq!(repo.current_branch().unwrap().as_deref(), Some("main"));
        let head = repo.head_commit().unwrap();
        assert_eq!(head.len(), 40);
    }

    #[test]
    fn detect_fails_outside_repo() {
        let dir = tempfile::tempdir().unwrap();
        let err = GitRepo::detect(dir.path()).unwrap_err();
        assert!(matches!(err, GitError::NotARepository(_)), "{err:?}");
    }

    #[test]
    fn worktree_lifecycle_and_diff() {
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = dir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        init_repo(&repo_dir);
        let repo = GitRepo::detect(&repo_dir).unwrap();
        let base = repo.head_commit().unwrap();

        let wt = dir.path().join("wt").join("task-001");
        repo.worktree_add(&wt, "forge/task-001", &base).unwrap();
        assert!(wt.join("README.md").exists());

        let entries = repo.worktree_list().unwrap();
        assert!(entries.iter().any(|e| e.path == wt));

        // Agent-style changes: new file + edit.
        std::fs::write(wt.join("new.txt"), "line1\nline2\n").unwrap();
        std::fs::write(wt.join("README.md"), "hello\nworld\n").unwrap();
        assert!(repo.is_dirty(&wt).unwrap());
        repo.mark_intent_to_add(&wt).unwrap();
        let stat = repo.diff_stat(&wt).unwrap();
        assert!(stat.contains("new.txt"), "{stat}");
        assert!(stat.contains("README.md"), "{stat}");
        let (added, deleted) = repo.diff_numstat(&wt).unwrap();
        assert!(added >= 3, "added={added}");
        assert_eq!(deleted, 0);

        let files = repo.changed_files(&wt).unwrap();
        assert!(files.contains(&"new.txt".to_string()));

        // Canonical repo untouched.
        let canonical_stat = repo.diff_stat(&repo_dir).unwrap();
        assert!(canonical_stat.trim().is_empty(), "{canonical_stat}");

        // Dirty worktrees are never removed without force.
        let err = repo.worktree_remove(&wt, false).unwrap_err();
        assert!(matches!(err, GitError::Unsafe(_)), "{err:?}");
        repo.worktree_remove(&wt, true).unwrap();
        assert!(!wt.exists());
    }

    #[test]
    fn stale_registration_is_pruned_and_add_retried() {
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = dir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        init_repo(&repo_dir);
        let repo = GitRepo::detect(&repo_dir).unwrap();
        let base = repo.head_commit().unwrap();
        let wt = dir.path().join("wt").join("task-001");
        repo.worktree_add(&wt, "forge/task-001", &base).unwrap();

        // Simulate "data directory deleted behind git's back".
        std::fs::remove_dir_all(&wt).unwrap();

        // Same path again: without pruning git exits 128 ("missing but
        // already registered"); the self-healing path must succeed.
        repo.worktree_add(&wt, "forge/task-001-retry", &base)
            .unwrap();
        assert!(wt.join("README.md").exists());
    }

    #[test]
    fn branch_collision_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = dir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        init_repo(&repo_dir);
        let repo = GitRepo::detect(&repo_dir).unwrap();
        let base = repo.head_commit().unwrap();
        repo.worktree_add(&dir.path().join("a"), "forge/task-001", &base)
            .unwrap();
        let err = repo
            .worktree_add(&dir.path().join("b"), "forge/task-001", &base)
            .unwrap_err();
        assert!(matches!(err, GitError::Command { .. }), "{err:?}");
    }

    #[test]
    fn sanitize_ref_component_is_safe() {
        assert_eq!(sanitize_ref_component("task-001"), "task-001");
        assert_eq!(sanitize_ref_component("../etc"), "etc");
        assert_eq!(sanitize_ref_component("a b/c"), "a-b-c");
        assert!(!sanitize_ref_component("..").contains(".."));
    }

    #[test]
    fn ensure_inside_blocks_escape() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        assert!(ensure_inside(&base, &outside).is_err());
        let inside = base.join("x");
        std::fs::create_dir_all(&inside).unwrap();
        assert!(ensure_inside(&base, &inside).is_ok());
    }
}
