//! Git collector: ingest commits from configured repos as events.
//!
//! We shell out to `git log` rather than depend on libgit2 — the binary is everywhere
//! and the output is stable. Each repo gets its own checkpoint storing the last-seen
//! HEAD SHA. First run is silent: we record HEAD without ingesting, so we don't
//! dump thousands of historical commits the first time a repo is wired up.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};

use urchin_core::{
    event::{Actor, Event, EventKind},
    identity::Identity,
    journal::Journal,
};

use crate::state::state_dir;

pub struct GitOpts {
    pub repo: PathBuf,
    pub checkpoint_path: PathBuf,
}

impl GitOpts {
    pub fn defaults_for(repo: PathBuf) -> Self {
        let checkpoint_path = default_checkpoint_path(&repo);
        Self {
            repo,
            checkpoint_path,
        }
    }
}

pub fn default_checkpoint_path(repo: &Path) -> PathBuf {
    let abs = fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    let safe = abs
        .display()
        .to_string()
        .replace(std::path::MAIN_SEPARATOR, "_")
        .trim_start_matches('_')
        .to_string();
    state_dir().join("git").join(format!("{}.checkpoint", safe))
}

/// Ingest new commits from a single repo. Returns the number of events appended.
pub fn collect_repo(journal: &Journal, identity: &Identity, opts: &GitOpts) -> Result<usize> {
    let repo = opts.repo.as_path();
    if !is_git_repo(repo) {
        return Err(anyhow!("not a git repo: {}", repo.display()));
    }

    let cp_path = &opts.checkpoint_path;
    let last_sha = fs::read_to_string(cp_path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let head = head_sha(repo)?;

    // First run: record HEAD without ingesting historical commits.
    if last_sha.is_none() {
        write_checkpoint(cp_path, &head)?;
        return Ok(0);
    }

    let range = format!("{}..HEAD", last_sha.as_deref().unwrap());
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("log")
        .arg(&range)
        .arg("--pretty=format:%H%x09%aI%x09%an%x09%s")
        .arg("--no-merges")
        .output()
        .context("git log failed to spawn")?;

    if !output.status.success() {
        return Err(anyhow!(
            "git log {} failed: {}",
            range,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut count = 0;

    // git log returns newest first; iterate reversed so events land oldest-first.
    let lines: Vec<&str> = stdout.lines().collect();
    for line in lines.iter().rev() {
        let parts: Vec<&str> = line.splitn(4, '\t').collect();
        if parts.len() < 4 {
            continue;
        }
        let (sha, date, author, subject) = (parts[0], parts[1], parts[2], parts[3]);

        let mut event = Event::new(
            "git",
            EventKind::Commit,
            format!("{} {}", short_sha(sha), subject),
        );
        event.title = Some(subject.to_string());
        event.workspace = Some(repo.display().to_string());
        event.tags = vec![format!("author:{}", author), format!("sha:{}", sha)];

        if let Ok(parsed) = DateTime::parse_from_rfc3339(date) {
            event.timestamp = parsed.with_timezone(&Utc);
        }

        event.actor = Some(Actor {
            account: Some(identity.account.clone()),
            device: Some(identity.device.clone()),
            workspace: Some(repo.display().to_string()),
            node_id: Some(identity.node_id.clone()),
        });

        journal.append(&event)?;
        count += 1;
    }

    write_checkpoint(cp_path, &head)?;
    journal.flush()?;
    Ok(count)
}

fn is_git_repo(path: &Path) -> bool {
    path.join(".git").exists()
}

fn head_sha(repo: &Path) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .context("git rev-parse failed to spawn")?;
    if !out.status.success() {
        return Err(anyhow!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn short_sha(sha: &str) -> &str {
    if sha.len() >= 8 {
        &sha[..8]
    } else {
        sha
    }
}

fn write_checkpoint(path: &Path, sha: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, sha)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn run(cmd: &str, dir: &Path) {
        let status = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "command failed: {}", cmd);
    }

    fn fixture_repo() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        run("git init -q -b main", dir.path());
        run("git config user.email 'test@example.com'", dir.path());
        run("git config user.name 'Test User'", dir.path());
        run("git config commit.gpgsign false", dir.path());
        run(
            "echo 'one' > a.txt && git add a.txt && git commit -qm 'initial'",
            dir.path(),
        );
        dir
    }

    fn fixture(
        repo: &TempDir,
    ) -> (
        GitOpts,
        Journal,
        Identity,
        tempfile::TempDir,
        tempfile::NamedTempFile,
    ) {
        let state = tempfile::tempdir().unwrap();
        let tmp_journal = tempfile::NamedTempFile::new().unwrap();
        let journal = Journal::new(tmp_journal.path().to_path_buf());
        let identity = Identity::for_test("t", "t");
        let opts = GitOpts {
            repo: repo.path().to_path_buf(),
            checkpoint_path: state.path().join("git-checkpoint"),
        };
        (opts, journal, identity, state, tmp_journal)
    }

    #[test]
    fn first_run_is_silent_and_records_checkpoint() {
        let repo = fixture_repo();
        let (opts, journal, identity, _state, _tmp) = fixture(&repo);

        let n = collect_repo(&journal, &identity, &opts).unwrap();
        assert_eq!(n, 0, "first run should not ingest anything");

        assert!(opts.checkpoint_path.exists());
        let head = head_sha(repo.path()).unwrap();
        assert_eq!(fs::read_to_string(&opts.checkpoint_path).unwrap(), head);
    }

    #[test]
    fn second_run_picks_up_new_commits() {
        let repo = fixture_repo();
        let (opts, journal, identity, _state, _tmp) = fixture(&repo);

        // First run: silent
        assert_eq!(collect_repo(&journal, &identity, &opts).unwrap(), 0);

        run(
            "echo 'two' > b.txt && git add b.txt && git commit -qm 'add b'",
            repo.path(),
        );
        run(
            "echo 'three' > c.txt && git add c.txt && git commit -qm 'add c'",
            repo.path(),
        );

        let n = collect_repo(&journal, &identity, &opts).unwrap();
        assert_eq!(n, 2);

        let events = journal.read_all().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].source, "git");
        assert!(events[0].content.contains("add b"));
        assert!(events[1].content.contains("add c"));
    }

    #[test]
    fn rejects_non_git_directory() {
        let dir = tempfile::tempdir().unwrap();
        let tmp_journal = tempfile::NamedTempFile::new().unwrap();
        let journal = Journal::new(tmp_journal.path().to_path_buf());
        let identity = Identity::for_test("t", "t");
        let opts = GitOpts {
            repo: dir.path().to_path_buf(),
            checkpoint_path: dir.path().join("checkpoint"),
        };

        let err = collect_repo(&journal, &identity, &opts).unwrap_err();
        assert!(err.to_string().contains("not a git repo"));
    }
}
