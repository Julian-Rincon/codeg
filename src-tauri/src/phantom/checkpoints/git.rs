//! Git plumbing for Phantom checkpoints. Every operation here leaves the
//! user's index, HEAD, branches and stash exactly as they were: snapshots are
//! built in a throwaway index (`GIT_INDEX_FILE`), stored as dangling commits
//! kept alive by refs under `refs/phantom/checkpoints/`, and a restore writes
//! the working tree through another throwaway index.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::AsyncWriteExt;

/// Identity stamped on snapshot commits, so `commit-tree` works in a repo with
/// no `user.name`/`user.email` configured.
const SNAPSHOT_IDENT_NAME: &str = "Phantom";
const SNAPSHOT_IDENT_EMAIL: &str = "phantom@localhost";

/// Mode git records for a submodule. A checkpoint never writes or deletes
/// one: its contents belong to another repository.
const GITLINK_MODE: &str = "160000";

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git {args} failed: {stderr}")]
    Failed { args: String, stderr: String },
    #[error("git could not run: {0}")]
    Io(#[from] std::io::Error),
}

pub type GitResult<T> = Result<T, GitError>;

/// Run `git <args>` in `root` with the inherited repo-selection variables
/// cleared (so a `GIT_DIR` from Phantom's own environment can never redirect
/// it) and optional locks off (so a read never rewrites the user's index).
async fn run(
    root: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    stdin: Option<&[u8]>,
) -> GitResult<Vec<u8>> {
    let mut cmd = crate::process::tokio_command("git");
    cmd.args(args)
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A timed-out snapshot drops this future; the child must go with it.
        .kill_on_drop(true);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;
    if let Some(input) = stdin {
        if let Some(mut pipe) = child.stdin.take() {
            pipe.write_all(input).await?;
            pipe.shutdown().await?;
        }
    }
    let output = child.wait_with_output().await?;
    if !output.status.success() {
        return Err(GitError::Failed {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(output.stdout)
}

async fn run_text(root: &Path, args: &[&str], envs: &[(&str, &str)]) -> GitResult<String> {
    let out = run(root, args, envs, None).await?;
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

/// The top level of the work tree containing `dir`, or `None` when `dir` is
/// not inside a git work tree (or git is missing) — the caller's cue to offer
/// conversation-only rewind.
pub async fn repo_root(dir: &Path) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    let out = run_text(dir, &["rev-parse", "--show-toplevel"], &[])
        .await
        .ok()?;
    (!out.is_empty()).then(|| PathBuf::from(out))
}

/// `HEAD`'s commit, or `None` on an unborn branch.
pub async fn head_commit(root: &Path) -> Option<String> {
    run_text(root, &["rev-parse", "--verify", "-q", "HEAD^{commit}"], &[])
        .await
        .ok()
        .filter(|s| !s.is_empty())
}

/// The tree a commit points at.
pub async fn tree_of(root: &Path, commit: &str) -> GitResult<String> {
    let spec = format!("{commit}^{{tree}}");
    run_text(root, &["rev-parse", "--verify", &spec], &[]).await
}

/// A scratch index file that removes itself (and any `.lock` git left
/// beside it) however the snapshot ends — including a timeout dropping it.
struct TempIndex {
    path: PathBuf,
}

impl TempIndex {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("phantom-checkpoint-{}.index", uuid::Uuid::new_v4()));
        Self { path }
    }

    fn env(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let mut lock = self.path.clone().into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(PathBuf::from(lock));
    }
}

/// Write the working tree as git would commit it with `git add -A` — tracked
/// files plus every untracked file `.gitignore` does not exclude — into a
/// tree object, without touching the user's index.
///
/// The scratch index starts as a COPY of the user's index rather than from
/// `read-tree HEAD`: both yield the same tree once `add -A` has run, but the
/// copy carries the cached stat data, so git only re-hashes files that
/// actually changed instead of every tracked file. The user's index is only
/// read. With no index yet (fresh clone mid-checkout, bare-ish states) it
/// falls back to `read-tree HEAD`, and on an unborn branch to empty.
pub async fn snapshot_tree(root: &Path) -> GitResult<String> {
    let temp = TempIndex::new();
    let index_env = temp.env();
    let envs = [("GIT_INDEX_FILE", index_env.as_str())];

    let user_index = run_text(root, &["rev-parse", "--git-path", "index"], &[])
        .await
        .ok()
        .map(|p| {
            let p = PathBuf::from(p);
            if p.is_absolute() {
                p
            } else {
                root.join(p)
            }
        });
    let seeded = match user_index {
        Some(p) if p.is_file() => std::fs::copy(&p, &temp.path).is_ok(),
        _ => false,
    };
    if !seeded && head_commit(root).await.is_some() {
        run(root, &["read-tree", "HEAD"], &envs, None).await?;
    }

    run(root, &["add", "-A"], &envs, None).await?;
    run_text(root, &["write-tree"], &envs).await
}

/// Store `tree` as a commit (parent: `parent`, normally HEAD) that no branch
/// points at. Never signs: a pinentry prompt here would hang the user's send.
pub async fn commit_tree(
    root: &Path,
    tree: &str,
    parent: Option<&str>,
    message: &str,
) -> GitResult<String> {
    let mut args = vec!["commit-tree", "--no-gpg-sign", tree, "-m", message];
    if let Some(p) = parent {
        args.push("-p");
        args.push(p);
    }
    let envs = [
        ("GIT_AUTHOR_NAME", SNAPSHOT_IDENT_NAME),
        ("GIT_AUTHOR_EMAIL", SNAPSHOT_IDENT_EMAIL),
        ("GIT_COMMITTER_NAME", SNAPSHOT_IDENT_NAME),
        ("GIT_COMMITTER_EMAIL", SNAPSHOT_IDENT_EMAIL),
    ];
    run_text(root, &args, &envs).await
}

pub async fn update_ref(root: &Path, name: &str, sha: &str) -> GitResult<()> {
    run(root, &["update-ref", name, sha], &[], None).await?;
    Ok(())
}

/// Delete every ref in `names` in one `update-ref --stdin` transaction.
/// Already-missing refs are not an error.
pub async fn delete_refs(root: &Path, names: &[String]) -> GitResult<()> {
    if names.is_empty() {
        return Ok(());
    }
    let mut input = String::new();
    for name in names {
        input.push_str("delete ");
        input.push_str(name);
        input.push('\n');
    }
    run(
        root,
        &["update-ref", "--stdin"],
        &[],
        Some(input.as_bytes()),
    )
    .await?;
    Ok(())
}

/// Every ref under `prefix` (e.g. `refs/phantom/checkpoints/12/`).
pub async fn list_refs(root: &Path, prefix: &str) -> GitResult<Vec<String>> {
    let out = run_text(root, &["for-each-ref", "--format=%(refname)", prefix], &[]).await?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// One path that differs between two trees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffEntry {
    /// `A`, `D`, `M` or `T`, read FROM `from` TO `to`.
    pub status: char,
    pub path: String,
    pub from_mode: String,
    pub to_mode: String,
}

/// `git diff-tree -r --raw` between two tree-ishes, renames off (a rename is
/// a delete plus an add, which is exactly what a restore has to do).
pub async fn diff_entries(root: &Path, from: &str, to: &str) -> GitResult<Vec<DiffEntry>> {
    let out = run(
        root,
        &["diff-tree", "-r", "-z", "--no-renames", "--raw", from, to],
        &[],
        None,
    )
    .await?;
    Ok(parse_raw_diff(&out))
}

/// Parse `-z --raw` output: `:<mode> <mode> <sha> <sha> <status>\0<path>\0`.
fn parse_raw_diff(out: &[u8]) -> Vec<DiffEntry> {
    let mut entries = Vec::new();
    let mut fields = out.split(|b| *b == 0).filter(|f| !f.is_empty());
    while let Some(meta) = fields.next() {
        let Some(path) = fields.next() else { break };
        let meta = String::from_utf8_lossy(meta);
        let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        if parts.len() < 5 {
            continue;
        }
        let status = parts[4].chars().next().unwrap_or('M');
        entries.push(DiffEntry {
            status,
            path: String::from_utf8_lossy(path).into_owned(),
            from_mode: parts[0].to_string(),
            to_mode: parts[1].to_string(),
        });
    }
    entries
}

/// Per-path `(additions, deletions)` between two tree-ishes; `None` counts
/// for binary files.
pub async fn diff_numstat(
    root: &Path,
    from: &str,
    to: &str,
) -> GitResult<HashMap<String, (Option<u32>, Option<u32>)>> {
    let out = run(
        root,
        &[
            "diff-tree",
            "-r",
            "-z",
            "--no-renames",
            "--numstat",
            from,
            to,
        ],
        &[],
        None,
    )
    .await?;
    let mut stats = HashMap::new();
    for record in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let line = String::from_utf8_lossy(record);
        let mut cols = line.splitn(3, '\t');
        let (Some(add), Some(del), Some(path)) = (cols.next(), cols.next(), cols.next()) else {
            continue;
        };
        stats.insert(path.to_string(), (add.parse().ok(), del.parse().ok()));
    }
    Ok(stats)
}

/// Number of paths that differ between two tree-ishes.
pub async fn count_changed(root: &Path, from: &str, to: &str) -> GitResult<usize> {
    let out = run(
        root,
        &[
            "diff-tree",
            "-r",
            "-z",
            "--no-renames",
            "--name-only",
            from,
            to,
        ],
        &[],
        None,
    )
    .await?;
    Ok(out.split(|b| *b == 0).filter(|r| !r.is_empty()).count())
}

/// Number of files in a tree (the "changed files" of a first checkpoint on
/// an unborn branch, where there is nothing to diff against).
pub async fn count_files(root: &Path, tree: &str) -> GitResult<usize> {
    let out = run(
        root,
        &["ls-tree", "-r", "-z", "--name-only", tree],
        &[],
        None,
    )
    .await?;
    Ok(out.split(|b| *b == 0).filter(|r| !r.is_empty()).count())
}

/// What a restore changed on disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreOutcome {
    pub written: usize,
    pub deleted: usize,
}

/// Make the working tree match `target_tree`, given `current_tree` (a fresh
/// [`snapshot_tree`] of it). Only paths that differ are touched:
///
/// * a path present now but not in the target (created after the
///   checkpoint) is deleted, then any directory that leaves empty;
/// * a path missing now, or different, is written from the target.
///
/// Ignored files are never in either snapshot, so they are never touched.
/// Neither is the index, HEAD or any branch: the writes go through a scratch
/// index that is thrown away. Submodules are skipped. Deletions run first so a
/// file↔directory swap has room to land.
pub async fn restore_worktree(
    root: &Path,
    target_tree: &str,
    current_tree: &str,
) -> GitResult<RestoreOutcome> {
    let entries = diff_entries(root, target_tree, current_tree).await?;
    let mut to_delete = Vec::new();
    let mut to_write = Vec::new();
    for e in entries {
        match e.status {
            // In the current tree only → created after the checkpoint.
            'A' => {
                if e.to_mode != GITLINK_MODE {
                    to_delete.push(e.path);
                }
            }
            // Missing now, or different now → bring back the target's copy.
            _ => {
                if e.from_mode != GITLINK_MODE && e.from_mode != "000000" {
                    to_write.push(e.path);
                }
            }
        }
    }

    let mut outcome = RestoreOutcome::default();
    for rel in &to_delete {
        let abs = root.join(rel);
        match std::fs::symlink_metadata(&abs) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                std::fs::remove_file(&abs)?;
                outcome.deleted += 1;
                prune_empty_parents(root, &abs);
            }
            Err(_) => {}
        }
    }

    if !to_write.is_empty() {
        let temp = TempIndex::new();
        let index_env = temp.env();
        let envs = [("GIT_INDEX_FILE", index_env.as_str())];
        run(root, &["read-tree", target_tree], &envs, None).await?;
        let mut input = Vec::new();
        for rel in &to_write {
            input.extend_from_slice(rel.as_bytes());
            input.push(0);
        }
        run(
            root,
            &["checkout-index", "-f", "-z", "--stdin"],
            &envs,
            Some(&input),
        )
        .await?;
        outcome.written = to_write.len();
    }
    Ok(outcome)
}

/// Remove the directories between `file` and `root` that are now empty —
/// stopping at the first one that still holds anything (an ignored file
/// included), and never removing `root` itself.
fn prune_empty_parents(root: &Path, file: &Path) {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if d == root || !d.starts_with(root) {
            break;
        }
        if std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_raw_diff_records() {
        let raw = b":100644 000000 aaaa 0000 D\0gone.txt\0:000000 100644 0000 bbbb A\0dir/new file.rs\0:100644 100755 aaaa aaaa M\0run.sh\0";
        let entries = parse_raw_diff(raw);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].status, 'D');
        assert_eq!(entries[0].path, "gone.txt");
        assert_eq!(entries[1].status, 'A');
        assert_eq!(entries[1].path, "dir/new file.rs");
        assert_eq!(entries[1].from_mode, "000000");
        assert_eq!(entries[2].to_mode, "100755");
    }
}
