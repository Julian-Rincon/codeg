//! Phantom's memory editor: read and write the Claude Code memory files
//! (`CLAUDE.md`) that apply to a workspace folder — the GUI twin of Claude
//! Code's `/memory` command and its `#` quick-add.
//!
//! The client never names a path. It names a workspace folder (which must be a
//! live row in the `folder` table) and a [`MemoryScope`]; the file each scope
//! maps to is fixed here:
//!
//! | scope                | file                          |
//! |----------------------|-------------------------------|
//! | `user`               | `~/.claude/CLAUDE.md`         |
//! | `project`            | `<folder>/CLAUDE.md`          |
//! | `project_dot_claude` | `<folder>/.claude/CLAUDE.md`  |
//! | `local`              | `<folder>/CLAUDE.local.md`    |
//!
//! A symlinked memory file (`CLAUDE.md -> AGENTS.md` is common) is followed,
//! but only while its real location stays inside the folder (project scopes)
//! or the home directory (user scope) — so nothing reachable through these
//! commands can be steered outside the files above.

use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};

use crate::app_error::AppCommandError;
use crate::db::entities::folder;

/// Largest memory file the editor reads or writes. Claude Code loads these
/// files into every prompt, so anything near this size is already a problem;
/// the cap only keeps a mistaken target from being slurped into the webview.
pub const MAX_MEMORY_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Longest single quick-add note (`# …` in the composer), in characters.
pub const MAX_QUICK_ADD_CHARS: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    User,
    Project,
    ProjectDotClaude,
    Local,
}

impl MemoryScope {
    pub const ALL: [MemoryScope; 4] = [
        MemoryScope::Project,
        MemoryScope::ProjectDotClaude,
        MemoryScope::Local,
        MemoryScope::User,
    ];
}

/// The two directories every memory path hangs off.
#[derive(Debug, Clone)]
pub struct MemoryRoots {
    pub home: PathBuf,
    pub folder: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryFileInfo {
    pub scope: MemoryScope,
    /// The path as Claude Code names it (not the symlink-resolved one).
    pub path: String,
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    /// Why this file can't be opened (e.g. a symlink leading outside the
    /// folder). The other scopes stay usable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryFileContent {
    pub scope: MemoryScope,
    pub path: String,
    pub exists: bool,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MemoryWriteResult {
    Saved {
        mtime_ms: i64,
    },
    /// The file on disk is not the one the editor loaded: it changed (or
    /// appeared, or vanished) since. Nothing was written.
    Conflict {
        #[serde(skip_serializing_if = "Option::is_none")]
        current_mtime_ms: Option<i64>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryAppendResult {
    pub scope: MemoryScope,
    pub path: String,
    pub mtime_ms: i64,
    /// The file did not exist and was created by this append.
    pub created: bool,
}

/// Where `scope`'s memory file lives, before any symlink is followed.
pub fn memory_file_path(scope: MemoryScope, roots: &MemoryRoots) -> PathBuf {
    match scope {
        MemoryScope::User => roots.home.join(".claude").join("CLAUDE.md"),
        MemoryScope::Project => roots.folder.join("CLAUDE.md"),
        MemoryScope::ProjectDotClaude => roots.folder.join(".claude").join("CLAUDE.md"),
        MemoryScope::Local => roots.folder.join("CLAUDE.local.md"),
    }
}

fn containment_root(scope: MemoryScope, roots: &MemoryRoots) -> &Path {
    match scope {
        MemoryScope::User => &roots.home,
        _ => &roots.folder,
    }
}

/// Accept a workspace folder path only if it is absolute, contains no `.`/`..`
/// components and is an existing directory. Returns it canonicalized.
pub fn validate_folder_root(raw: &str) -> Result<PathBuf, AppCommandError> {
    let path = Path::new(raw);
    if raw.is_empty() || !path.is_absolute() {
        return Err(AppCommandError::invalid_input(
            "workspace folder path must be absolute",
        ));
    }
    if path
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(AppCommandError::invalid_input(
            "workspace folder path must not contain `.` or `..`",
        ));
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|_| AppCommandError::not_found("workspace folder does not exist"))?;
    if !canonical.is_dir() {
        return Err(AppCommandError::invalid_input(
            "workspace folder is not a directory",
        ));
    }
    Ok(canonical)
}

/// The real file a scope reads from / writes to. An existing file (symlink or
/// not) is canonicalized; a missing one is placed under its canonicalized
/// parent, which may itself be missing (`.claude/` not created yet), in which
/// case the grandparent is checked instead. Either way the result must stay
/// under the scope's (canonical) containment root.
pub fn resolve_target(scope: MemoryScope, roots: &MemoryRoots) -> Result<PathBuf, AppCommandError> {
    let root = std::fs::canonicalize(containment_root(scope, roots))
        .map_err(|_| AppCommandError::not_found("memory root directory does not exist"))?;
    let nominal = memory_file_path(scope, roots);
    let file_name = nominal
        .file_name()
        .expect("memory paths always end in a file name")
        .to_owned();

    let resolved = match std::fs::symlink_metadata(&nominal) {
        Ok(_) => std::fs::canonicalize(&nominal)
            .map_err(|_| AppCommandError::invalid_input("memory file is a broken symlink"))?,
        Err(_) => {
            let parent = nominal.parent().expect("memory paths have a parent");
            match std::fs::canonicalize(parent) {
                Ok(dir) => dir.join(&file_name),
                Err(_) => {
                    // Only the `.claude` directory may be missing; its own
                    // parent (the folder or home) was canonicalized above.
                    let grand = parent.parent().expect("`.claude` has a parent");
                    let dir_name = parent.file_name().expect("`.claude` has a name");
                    std::fs::canonicalize(grand)
                        .map_err(|_| AppCommandError::not_found("memory directory does not exist"))?
                        .join(dir_name)
                        .join(&file_name)
                }
            }
        }
    };

    if !resolved.starts_with(&root) {
        return Err(AppCommandError::permission_denied(
            "memory file resolves outside its allowed directory",
        ));
    }
    if resolved.exists() && !resolved.is_file() {
        return Err(AppCommandError::invalid_input(
            "memory path exists but is not a regular file",
        ));
    }
    Ok(resolved)
}

fn mtime_ms(meta: &std::fs::Metadata) -> Option<i64> {
    meta.modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

fn display_path(scope: MemoryScope, roots: &MemoryRoots) -> String {
    memory_file_path(scope, roots)
        .to_string_lossy()
        .into_owned()
}

/// Normalize a quick-add note: one non-empty line, trimmed, bounded.
pub fn normalize_quick_add_text(text: &str) -> Result<String, AppCommandError> {
    if text.contains('\n') || text.contains('\r') {
        return Err(AppCommandError::invalid_input(
            "quick-add note must be a single line",
        ));
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(AppCommandError::invalid_input("quick-add note is empty"));
    }
    if trimmed.chars().count() > MAX_QUICK_ADD_CHARS {
        return Err(AppCommandError::invalid_input("quick-add note is too long"));
    }
    Ok(trimmed.to_string())
}

/// `existing` with `- <note>` appended as its own line.
pub fn append_bullet(existing: &str, note: &str) -> String {
    let mut out = String::with_capacity(existing.len() + note.len() + 4);
    out.push_str(existing);
    if !existing.is_empty() && !existing.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("- ");
    out.push_str(note);
    out.push('\n');
    out
}

fn read_bounded(path: &Path) -> Result<String, AppCommandError> {
    let meta = std::fs::metadata(path).map_err(AppCommandError::io)?;
    if meta.len() > MAX_MEMORY_FILE_BYTES {
        return Err(AppCommandError::invalid_input("memory file is too large"));
    }
    let bytes = std::fs::read(path).map_err(AppCommandError::io)?;
    String::from_utf8(bytes)
        .map_err(|_| AppCommandError::invalid_input("memory file is not valid UTF-8"))
}

/// Write via a sibling temp file + rename so a crash never leaves a truncated
/// CLAUDE.md. Creates the `.claude/` directory when that is all that's missing.
fn write_atomic(target: &Path, content: &str) -> Result<i64, AppCommandError> {
    let dir = target
        .parent()
        .ok_or_else(|| AppCommandError::invalid_input("memory path has no parent"))?;
    if !dir.exists() {
        std::fs::create_dir(dir).map_err(AppCommandError::io)?;
    }
    let file_name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let nanos = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(
        ".{file_name}.phantom-{}-{nanos}.tmp",
        std::process::id()
    ));
    std::fs::write(&tmp, content.as_bytes()).map_err(AppCommandError::io)?;
    if let Ok(meta) = std::fs::metadata(target) {
        // Keep the original file's mode (e.g. a read-for-group CLAUDE.md).
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    if let Err(err) = std::fs::rename(&tmp, target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(AppCommandError::io(err));
    }
    let meta = std::fs::metadata(target).map_err(AppCommandError::io)?;
    Ok(mtime_ms(&meta).unwrap_or(0))
}

pub fn list_memory_files(roots: &MemoryRoots) -> Vec<MemoryFileInfo> {
    MemoryScope::ALL
        .iter()
        .map(|&scope| {
            let path = display_path(scope, roots);
            match resolve_target(scope, roots) {
                Ok(target) => {
                    let meta = std::fs::metadata(&target).ok();
                    MemoryFileInfo {
                        scope,
                        path,
                        exists: meta.is_some(),
                        mtime_ms: meta.as_ref().and_then(mtime_ms),
                        size_bytes: meta.as_ref().map(|m| m.len()),
                        error: None,
                    }
                }
                Err(err) => MemoryFileInfo {
                    scope,
                    path,
                    exists: false,
                    mtime_ms: None,
                    size_bytes: None,
                    error: Some(err.message.clone()),
                },
            }
        })
        .collect()
}

pub fn read_memory_file(
    scope: MemoryScope,
    roots: &MemoryRoots,
) -> Result<MemoryFileContent, AppCommandError> {
    let target = resolve_target(scope, roots)?;
    let path = display_path(scope, roots);
    match std::fs::metadata(&target) {
        Ok(meta) => Ok(MemoryFileContent {
            scope,
            path,
            exists: true,
            content: read_bounded(&target)?,
            mtime_ms: mtime_ms(&meta),
        }),
        Err(_) => Ok(MemoryFileContent {
            scope,
            path,
            exists: false,
            content: String::new(),
            mtime_ms: None,
        }),
    }
}

/// Save the editor's buffer. `expected_mtime_ms` is the mtime the editor
/// loaded (`None` = it loaded a missing file); any mismatch is a conflict and
/// nothing is written.
pub fn write_memory_file(
    scope: MemoryScope,
    roots: &MemoryRoots,
    content: &str,
    expected_mtime_ms: Option<i64>,
) -> Result<MemoryWriteResult, AppCommandError> {
    if content.len() as u64 > MAX_MEMORY_FILE_BYTES {
        return Err(AppCommandError::invalid_input("memory file is too large"));
    }
    let target = resolve_target(scope, roots)?;
    let current = std::fs::metadata(&target).ok().map(|m| mtime_ms(&m));
    let unchanged = match (current, expected_mtime_ms) {
        (None, None) => true,
        (Some(on_disk), Some(expected)) => on_disk == Some(expected),
        _ => false,
    };
    if !unchanged {
        return Ok(MemoryWriteResult::Conflict {
            current_mtime_ms: current.flatten(),
        });
    }
    let mtime_ms = write_atomic(&target, content)?;
    Ok(MemoryWriteResult::Saved { mtime_ms })
}

pub fn append_memory_note(
    scope: MemoryScope,
    roots: &MemoryRoots,
    text: &str,
) -> Result<MemoryAppendResult, AppCommandError> {
    let note = normalize_quick_add_text(text)?;
    let target = resolve_target(scope, roots)?;
    let created = !target.exists();
    let existing = if created {
        String::new()
    } else {
        read_bounded(&target)?
    };
    let next = append_bullet(&existing, &note);
    if next.len() as u64 > MAX_MEMORY_FILE_BYTES {
        return Err(AppCommandError::invalid_input("memory file is too large"));
    }
    let mtime_ms = write_atomic(&target, &next)?;
    Ok(MemoryAppendResult {
        scope,
        path: display_path(scope, roots),
        mtime_ms,
        created,
    })
}

/// Resolve the roots for a request: the folder must be a live (not deleted)
/// workspace folder row, stored under exactly this path.
pub async fn resolve_roots(
    conn: &DatabaseConnection,
    folder_path: &str,
) -> Result<MemoryRoots, AppCommandError> {
    let known = folder::Entity::find()
        .filter(folder::Column::Path.eq(folder_path))
        .filter(folder::Column::DeletedAt.is_null())
        .one(conn)
        .await
        .map_err(|e| AppCommandError::database_error(e.to_string()))?;
    if known.is_none() {
        return Err(AppCommandError::permission_denied(
            "not a known workspace folder",
        ));
    }
    let folder = validate_folder_root(folder_path)?;
    let home = dirs::home_dir()
        .ok_or_else(|| AppCommandError::configuration_missing("home directory is unknown"))?;
    Ok(MemoryRoots { home, folder })
}

pub async fn phantom_memory_list_core(
    conn: &DatabaseConnection,
    folder_path: &str,
) -> Result<Vec<MemoryFileInfo>, AppCommandError> {
    let roots = resolve_roots(conn, folder_path).await?;
    Ok(list_memory_files(&roots))
}

pub async fn phantom_memory_read_core(
    conn: &DatabaseConnection,
    folder_path: &str,
    scope: MemoryScope,
) -> Result<MemoryFileContent, AppCommandError> {
    let roots = resolve_roots(conn, folder_path).await?;
    read_memory_file(scope, &roots)
}

pub async fn phantom_memory_write_core(
    conn: &DatabaseConnection,
    folder_path: &str,
    scope: MemoryScope,
    content: &str,
    expected_mtime_ms: Option<i64>,
) -> Result<MemoryWriteResult, AppCommandError> {
    let roots = resolve_roots(conn, folder_path).await?;
    write_memory_file(scope, &roots, content, expected_mtime_ms)
}

pub async fn phantom_memory_append_core(
    conn: &DatabaseConnection,
    folder_path: &str,
    scope: MemoryScope,
    text: &str,
) -> Result<MemoryAppendResult, AppCommandError> {
    let roots = resolve_roots(conn, folder_path).await?;
    append_memory_note(scope, &roots, text)
}

#[cfg(feature = "tauri-runtime")]
#[cfg_attr(feature = "tauri-runtime", tauri::command)]
pub async fn phantom_memory_list(
    db: tauri::State<'_, crate::db::AppDatabase>,
    folder_path: String,
) -> Result<Vec<MemoryFileInfo>, AppCommandError> {
    phantom_memory_list_core(&db.conn, &folder_path).await
}

#[cfg(feature = "tauri-runtime")]
#[cfg_attr(feature = "tauri-runtime", tauri::command)]
pub async fn phantom_memory_read(
    db: tauri::State<'_, crate::db::AppDatabase>,
    folder_path: String,
    scope: MemoryScope,
) -> Result<MemoryFileContent, AppCommandError> {
    phantom_memory_read_core(&db.conn, &folder_path, scope).await
}

#[cfg(feature = "tauri-runtime")]
#[cfg_attr(feature = "tauri-runtime", tauri::command)]
pub async fn phantom_memory_write(
    db: tauri::State<'_, crate::db::AppDatabase>,
    folder_path: String,
    scope: MemoryScope,
    content: String,
    expected_mtime_ms: Option<i64>,
) -> Result<MemoryWriteResult, AppCommandError> {
    phantom_memory_write_core(&db.conn, &folder_path, scope, &content, expected_mtime_ms).await
}

#[cfg(feature = "tauri-runtime")]
#[cfg_attr(feature = "tauri-runtime", tauri::command)]
pub async fn phantom_memory_append(
    db: tauri::State<'_, crate::db::AppDatabase>,
    folder_path: String,
    scope: MemoryScope,
    text: String,
) -> Result<MemoryAppendResult, AppCommandError> {
    phantom_memory_append_core(&db.conn, &folder_path, scope, &text).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct Fixture {
        _tmp: TempDir,
        roots: MemoryRoots,
        outside: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = TempDir::new().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let home = base.join("home");
        let folder = home.join("proj");
        let outside = base.join("outside");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        Fixture {
            _tmp: tmp,
            roots: MemoryRoots { home, folder },
            outside,
        }
    }

    #[test]
    fn scopes_map_to_exactly_the_four_memory_files() {
        let roots = MemoryRoots {
            home: PathBuf::from("/h"),
            folder: PathBuf::from("/w/p"),
        };
        assert_eq!(
            memory_file_path(MemoryScope::User, &roots),
            PathBuf::from("/h/.claude/CLAUDE.md")
        );
        assert_eq!(
            memory_file_path(MemoryScope::Project, &roots),
            PathBuf::from("/w/p/CLAUDE.md")
        );
        assert_eq!(
            memory_file_path(MemoryScope::ProjectDotClaude, &roots),
            PathBuf::from("/w/p/.claude/CLAUDE.md")
        );
        assert_eq!(
            memory_file_path(MemoryScope::Local, &roots),
            PathBuf::from("/w/p/CLAUDE.local.md")
        );
    }

    #[test]
    fn scope_wire_names_are_fixed_and_unknown_ones_rejected() {
        let s: MemoryScope = serde_json::from_str("\"project_dot_claude\"").unwrap();
        assert_eq!(s, MemoryScope::ProjectDotClaude);
        assert!(serde_json::from_str::<MemoryScope>("\"../etc/passwd\"").is_err());
        assert!(serde_json::from_str::<MemoryScope>("\"AGENTS.md\"").is_err());
    }

    #[test]
    fn folder_root_must_be_absolute_existing_dir_without_dot_segments() {
        let f = fixture();
        let ok = f.roots.folder.to_string_lossy().into_owned();
        assert_eq!(validate_folder_root(&ok).unwrap(), f.roots.folder);

        assert!(validate_folder_root("").is_err());
        assert!(validate_folder_root("relative/dir").is_err());
        let dotdot = format!("{ok}/../proj");
        assert!(validate_folder_root(&dotdot).is_err());
        let missing = format!("{ok}/nope");
        assert!(validate_folder_root(&missing).is_err());
        let file = f.roots.folder.join("CLAUDE.md");
        std::fs::write(&file, "x").unwrap();
        assert!(validate_folder_root(&file.to_string_lossy()).is_err());
    }

    #[test]
    fn missing_files_resolve_inside_their_root_including_missing_dot_claude() {
        let f = fixture();
        for scope in MemoryScope::ALL {
            let target = resolve_target(scope, &f.roots).unwrap();
            assert_eq!(target, memory_file_path(scope, &f.roots));
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_inside_the_folder_is_followed() {
        let f = fixture();
        let agents = f.roots.folder.join("AGENTS.md");
        std::fs::write(&agents, "shared\n").unwrap();
        std::os::unix::fs::symlink(&agents, f.roots.folder.join("CLAUDE.md")).unwrap();
        assert_eq!(
            resolve_target(MemoryScope::Project, &f.roots).unwrap(),
            agents
        );

        let read = read_memory_file(MemoryScope::Project, &f.roots).unwrap();
        assert!(read.exists);
        assert_eq!(read.content, "shared\n");
        assert_eq!(
            read.path,
            f.roots.folder.join("CLAUDE.md").to_string_lossy()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escaping_the_folder_is_rejected() {
        let f = fixture();
        let secret = f.outside.join("secret.txt");
        std::fs::write(&secret, "nope").unwrap();
        std::os::unix::fs::symlink(&secret, f.roots.folder.join("CLAUDE.local.md")).unwrap();
        assert!(resolve_target(MemoryScope::Local, &f.roots).is_err());
        assert!(read_memory_file(MemoryScope::Local, &f.roots).is_err());
        assert!(write_memory_file(MemoryScope::Local, &f.roots, "x", None).is_err());
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "nope");

        let listed = list_memory_files(&f.roots);
        let local = listed
            .iter()
            .find(|i| i.scope == MemoryScope::Local)
            .unwrap();
        assert!(local.error.is_some());
        assert!(!local.exists);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_dot_claude_dir_escaping_the_folder_is_rejected() {
        let f = fixture();
        std::os::unix::fs::symlink(&f.outside, f.roots.folder.join(".claude")).unwrap();
        assert!(resolve_target(MemoryScope::ProjectDotClaude, &f.roots).is_err());
        assert!(append_memory_note(MemoryScope::ProjectDotClaude, &f.roots, "x").is_err());
        assert!(!f.outside.join("CLAUDE.md").exists());
    }

    #[test]
    fn a_directory_named_claude_md_is_not_a_memory_file() {
        let f = fixture();
        std::fs::create_dir(f.roots.folder.join("CLAUDE.md")).unwrap();
        assert!(resolve_target(MemoryScope::Project, &f.roots).is_err());
    }

    #[test]
    fn write_creates_then_detects_conflicts_by_mtime() {
        let f = fixture();
        // Creating requires "expected missing".
        let saved =
            write_memory_file(MemoryScope::ProjectDotClaude, &f.roots, "a\n", None).unwrap();
        let MemoryWriteResult::Saved { mtime_ms } = saved else {
            panic!("expected save, got {saved:?}");
        };
        let path = f.roots.folder.join(".claude").join("CLAUDE.md");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n");

        // A second "create" sees the file that now exists.
        let again =
            write_memory_file(MemoryScope::ProjectDotClaude, &f.roots, "b\n", None).unwrap();
        assert_eq!(
            again,
            MemoryWriteResult::Conflict {
                current_mtime_ms: Some(mtime_ms)
            }
        );

        // A stale mtime conflicts and writes nothing.
        let stale = write_memory_file(
            MemoryScope::ProjectDotClaude,
            &f.roots,
            "c\n",
            Some(mtime_ms - 5_000),
        )
        .unwrap();
        assert!(matches!(stale, MemoryWriteResult::Conflict { .. }));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n");

        // The loaded mtime saves.
        let ok = write_memory_file(
            MemoryScope::ProjectDotClaude,
            &f.roots,
            "d\n",
            Some(mtime_ms),
        )
        .unwrap();
        assert!(matches!(ok, MemoryWriteResult::Saved { .. }));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "d\n");

        // Deleted under the editor's feet: expected Some, on disk none.
        std::fs::remove_file(&path).unwrap();
        let gone = write_memory_file(
            MemoryScope::ProjectDotClaude,
            &f.roots,
            "e\n",
            Some(mtime_ms),
        )
        .unwrap();
        assert_eq!(
            gone,
            MemoryWriteResult::Conflict {
                current_mtime_ms: None
            }
        );
    }

    #[test]
    fn write_leaves_no_temp_files_behind() {
        let f = fixture();
        write_memory_file(MemoryScope::Project, &f.roots, "x", None).unwrap();
        let names: Vec<_> = std::fs::read_dir(&f.roots.folder)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["CLAUDE.md".to_string()]);
    }

    #[test]
    fn append_bullet_keeps_lines_separate() {
        assert_eq!(append_bullet("", "use pnpm"), "- use pnpm\n");
        assert_eq!(append_bullet("# Notes\n", "x"), "# Notes\n- x\n");
        assert_eq!(append_bullet("# Notes", "x"), "# Notes\n- x\n");
    }

    #[test]
    fn quick_add_text_must_be_one_non_empty_bounded_line() {
        assert_eq!(normalize_quick_add_text("  use pnpm ").unwrap(), "use pnpm");
        assert!(normalize_quick_add_text("   ").is_err());
        assert!(normalize_quick_add_text("a\nb").is_err());
        assert!(normalize_quick_add_text("a\rb").is_err());
        assert!(normalize_quick_add_text(&"x".repeat(MAX_QUICK_ADD_CHARS + 1)).is_err());
    }

    #[test]
    fn append_creates_user_memory_and_dot_claude_dir() {
        let f = fixture();
        let first = append_memory_note(MemoryScope::User, &f.roots, "prefer tabs").unwrap();
        assert!(first.created);
        let second = append_memory_note(MemoryScope::User, &f.roots, "no emojis").unwrap();
        assert!(!second.created);
        let path = f.roots.home.join(".claude").join("CLAUDE.md");
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "- prefer tabs\n- no emojis\n"
        );
    }

    #[test]
    fn list_reports_existence_per_scope() {
        let f = fixture();
        std::fs::write(f.roots.folder.join("CLAUDE.md"), "hi").unwrap();
        let listed = list_memory_files(&f.roots);
        assert_eq!(listed.len(), 4);
        for info in &listed {
            assert!(info.error.is_none(), "{info:?}");
            assert_eq!(info.exists, info.scope == MemoryScope::Project);
        }
        let project = listed
            .iter()
            .find(|i| i.scope == MemoryScope::Project)
            .unwrap();
        assert_eq!(project.size_bytes, Some(2));
        assert!(project.mtime_ms.is_some());
    }

    #[test]
    fn oversized_writes_are_refused() {
        let f = fixture();
        let big = "x".repeat(MAX_MEMORY_FILE_BYTES as usize + 1);
        assert!(write_memory_file(MemoryScope::Project, &f.roots, &big, None).is_err());
        assert!(!f.roots.folder.join("CLAUDE.md").exists());
    }

    /// The folder comes from the client, so it must be a live row in the
    /// `folder` table before any path is derived from it — a path the client
    /// invents (a sibling, a traversal, an unrelated directory) has no memory
    /// files at all.
    #[tokio::test]
    async fn only_a_live_workspace_folder_row_unlocks_memory_files() {
        use crate::db::service::folder_service;
        use crate::db::test_helpers::{fresh_in_memory_db, seed_folder};

        let db = fresh_in_memory_db().await;
        let tmp = TempDir::new().unwrap();
        let folder = std::fs::canonicalize(tmp.path()).unwrap();
        let path = folder.to_string_lossy().into_owned();
        std::fs::create_dir(folder.join("sub")).unwrap();

        let refused = |err: AppCommandError| {
            assert!(
                matches!(err.code, crate::app_error::AppErrorCode::PermissionDenied),
                "{err:?}"
            );
        };
        // Not registered yet.
        refused(resolve_roots(&db.conn, &path).await.unwrap_err());
        // A real directory the client names anyway: still no row.
        refused(
            resolve_roots(&db.conn, &format!("{path}/sub"))
                .await
                .unwrap_err(),
        );
        // `..` spelled out: the same folder on disk, a different string.
        refused(
            resolve_roots(
                &db.conn,
                &format!(
                    "{path}/../{}",
                    folder.file_name().unwrap().to_string_lossy()
                ),
            )
            .await
            .unwrap_err(),
        );

        let id = seed_folder(&db, &path).await;
        let roots = resolve_roots(&db.conn, &path).await.unwrap();
        assert_eq!(roots.folder, folder);

        // Read/write stay inside the registered folder.
        let appended =
            phantom_memory_append_core(&db.conn, &path, MemoryScope::Project, "use pnpm")
                .await
                .unwrap();
        assert_eq!(
            appended.path,
            folder.join("CLAUDE.md").to_string_lossy().into_owned()
        );
        assert_eq!(
            std::fs::read_to_string(folder.join("CLAUDE.md")).unwrap(),
            "- use pnpm\n"
        );
        assert!(!folder.join("sub").join("CLAUDE.md").exists());
        // The project memory is the one this append created (the user scope's
        // `~/.claude/CLAUDE.md` is the machine's own and not asserted on).
        let listed = phantom_memory_list_core(&db.conn, &path).await.unwrap();
        let project = listed
            .iter()
            .find(|info| info.scope == MemoryScope::Project)
            .expect("project scope is always listed");
        assert!(project.exists);
        assert_eq!(project.path, folder.join("CLAUDE.md").to_string_lossy());
        for scope in [MemoryScope::ProjectDotClaude, MemoryScope::Local] {
            let info = listed.iter().find(|i| i.scope == scope).unwrap();
            assert!(!info.exists, "{info:?}");
        }

        let read = phantom_memory_read_core(&db.conn, &path, MemoryScope::Project)
            .await
            .unwrap();
        assert_eq!(read.content, "- use pnpm\n");
        let saved = phantom_memory_write_core(
            &db.conn,
            &path,
            MemoryScope::Project,
            "# notes\n",
            read.mtime_ms,
        )
        .await
        .unwrap();
        assert!(matches!(saved, MemoryWriteResult::Saved { .. }));

        // A deleted folder closes the door again.
        folder_service::soft_delete_folder(&db.conn, id)
            .await
            .unwrap();
        refused(resolve_roots(&db.conn, &path).await.unwrap_err());
    }
}
