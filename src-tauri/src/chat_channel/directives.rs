//! `[[phantom:send_file <path> | <caption>]]` directive parsing and the path
//! policy that gates which files an agent may push back through a chat
//! channel.
//!
//! An agent's final turn text for a Telegram-originated conversation can ask
//! Codeg to attach a file by writing one directive line (see the chat
//! preamble in [`super::chat_preamble`]). The reply hook in
//! `session_event_subscriber` strips every directive out of the text before
//! it is shown to the user and attempts to upload each referenced file
//! separately. Parsing never touches the filesystem; validation
//! ([`validate_send_file_path`]) is the only place that does, and it is the
//! sole gate between an LLM-controlled string and a real upload.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

/// Hard cap on an uploaded file's size (Telegram's own document limit for
/// bot uploads is 50 MB over the Bot API).
pub const MAX_SEND_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// One `[[phantom:send_file <path> | <caption>]]` directive extracted from an
/// agent's reply text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendFileDirective {
    /// Raw path exactly as written by the agent — not yet validated.
    pub path: String,
    /// Optional caption text after the `|` separator, trimmed.
    pub caption: Option<String>,
}

/// How an attachment's file extension maps to a channel backend's upload
/// method (Telegram distinguishes `sendPhoto` / `sendAudio` / `sendDocument`;
/// other backends can ignore the distinction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    Photo,
    Audio,
    Document,
}

/// Classify a file name by extension for attachment upload. Unknown or
/// missing extensions fall back to a generic document.
pub fn classify_attachment(filename: &str) -> AttachmentKind {
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "webp" => AttachmentKind::Photo,
        "mp3" | "wav" | "ogg" | "opus" | "m4a" | "flac" => AttachmentKind::Audio,
        _ => AttachmentKind::Document,
    }
}

static DIRECTIVE_RE: LazyLock<Regex> = LazyLock::new(|| {
    // `[[phantom:send_file <path>]]` or `[[phantom:send_file <path> | <caption>]]`.
    // The path is greedy-but-bounded to the `|` or the closing `]]`; both
    // sides are trimmed after the match.
    Regex::new(r"\[\[phantom:send_file\s+([^\|\]]+?)(?:\s*\|\s*([^\]]*?))?\s*\]\]")
        .expect("static directive regex is valid")
});

/// Remove every `send_file` directive from `text`, returning the cleaned text
/// (directive lines collapsed, not merely blanked) alongside the directives
/// found, in the order they appeared.
///
/// A directive with an empty path (`[[phantom:send_file ]]`) is treated as
/// malformed and left in place rather than parsed — this keeps
/// [`validate_send_file_path`] the only path-shaped decision point instead of
/// silently swallowing an agent's typo.
pub fn parse_send_file_directives(text: &str) -> (String, Vec<SendFileDirective>) {
    let mut directives = Vec::new();
    let mut cleaned = String::with_capacity(text.len());
    let mut cursor = 0;

    for cap in DIRECTIVE_RE.captures_iter(text) {
        let whole = cap.get(0).expect("group 0 always matches");
        let path = cap.get(1).map(|m| m.as_str().trim()).unwrap_or("");
        if path.is_empty() {
            continue;
        }
        let caption = cap
            .get(2)
            .map(|m| m.as_str().trim())
            .filter(|s| !s.is_empty())
            .map(String::from);

        cleaned.push_str(&text[cursor..whole.start()]);
        cursor = whole.end();
        directives.push(SendFileDirective {
            path: path.to_string(),
            caption,
        });
    }
    cleaned.push_str(&text[cursor..]);

    // Directive lines are typically alone on their own line; a removed
    // directive leaves its line's newlines behind, so collapse the run of
    // blank lines that creates down to one (only when something was actually
    // removed — untouched text keeps whatever blank-line structure the agent
    // wrote).
    let cleaned = if directives.is_empty() {
        cleaned
    } else {
        collapse_blank_lines(&cleaned)
    };
    (cleaned, directives)
}

/// Collapse a run of 2+ consecutive (blank) lines down to exactly one blank
/// line, and trim the whole result — used only after a directive was
/// actually removed, on the text that removal leaves behind.
static BLANK_RUN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\n[ \t]*(?:\n[ \t]*)+").expect("static blank-run regex is valid")
});

fn collapse_blank_lines(text: &str) -> String {
    BLANK_RUN_RE.replace_all(text, "\n\n").trim().to_string()
}

/// Why a candidate `send_file` path was rejected before any upload was
/// attempted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathPolicyError {
    #[error("path must be absolute: {0}")]
    NotAbsolute(String),
    #[error("path could not be resolved: {0}")]
    Unresolvable(String),
    #[error("path does not exist: {0}")]
    NotFound(String),
    #[error("path is not a regular file: {0}")]
    NotAFile(String),
    #[error("file is too large ({0} bytes, limit {1} bytes)")]
    TooLarge(u64, u64),
    #[error("path is outside the home directory: {0}")]
    OutsideHome(String),
    #[error("path is inside a protected directory: {0}")]
    DeniedDirectory(String),
    #[error("file name is not allowed: {0}")]
    DeniedName(String),
    #[error("home directory could not be determined")]
    NoHomeDirectory,
}

/// Subdirectories of `$HOME` that are never eligible for `send_file`, even
/// though they live under the home tree — credential stores and codeg's own
/// config/token database.
const DENIED_SUBDIRS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".config/codeg",
    ".hermes",
    // Not in the spec's literal list, but this is where codeg keeps its own
    // sqlite DB and channel tokens (see `paths::codeg_home_dir`) — denying it
    // serves the same intent the rest of the list protects.
    ".codeg",
];

/// File name patterns that are never eligible for `send_file`, regardless of
/// directory.
fn is_denied_name(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    lower.starts_with(".env")
        || lower.ends_with(".pem")
        || lower.ends_with(".key")
        || lower.starts_with("id_")
}

/// Validate and canonicalize an agent-supplied `send_file` path against the
/// policy: absolute, resolvable, an existing regular file, inside `$HOME`,
/// within the size cap, and outside every denied directory/name pattern.
///
/// `home` is injected (rather than read from `dirs::home_dir()` internally)
/// so tests can point it at a tempdir. Canonicalization happens BEFORE the
/// containment check, which is what makes this immune to a `..` component or
/// a symlink whose target escapes `$HOME`.
pub fn validate_send_file_path(raw: &str, home: &Path) -> Result<(PathBuf, u64), PathPolicyError> {
    let candidate = Path::new(raw.trim());
    if !candidate.is_absolute() {
        return Err(PathPolicyError::NotAbsolute(raw.to_string()));
    }

    let canonical = std::fs::canonicalize(candidate)
        .map_err(|_| PathPolicyError::Unresolvable(raw.to_string()))?;

    let home_canonical =
        std::fs::canonicalize(home).map_err(|_| PathPolicyError::NoHomeDirectory)?;

    if !canonical.starts_with(&home_canonical) {
        return Err(PathPolicyError::OutsideHome(
            canonical.display().to_string(),
        ));
    }

    let relative = canonical
        .strip_prefix(&home_canonical)
        .expect("checked with starts_with above");
    let relative_str = relative.to_string_lossy().replace('\\', "/");
    for denied in DENIED_SUBDIRS {
        if relative_str == *denied || relative_str.starts_with(&format!("{denied}/")) {
            return Err(PathPolicyError::DeniedDirectory(
                canonical.display().to_string(),
            ));
        }
    }

    let file_name = canonical
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| PathPolicyError::NotAFile(canonical.display().to_string()))?;
    if is_denied_name(file_name) {
        return Err(PathPolicyError::DeniedName(file_name.to_string()));
    }

    let metadata = std::fs::metadata(&canonical)
        .map_err(|_| PathPolicyError::NotFound(canonical.display().to_string()))?;
    if !metadata.is_file() {
        return Err(PathPolicyError::NotAFile(canonical.display().to_string()));
    }

    let size = metadata.len();
    if size > MAX_SEND_FILE_BYTES {
        return Err(PathPolicyError::TooLarge(size, MAX_SEND_FILE_BYTES));
    }

    Ok((canonical, size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    // ── attachment classification ──

    #[test]
    fn classifies_images_and_audio_and_falls_back_to_document() {
        assert_eq!(classify_attachment("photo.PNG"), AttachmentKind::Photo);
        assert_eq!(classify_attachment("clip.jpeg"), AttachmentKind::Photo);
        assert_eq!(classify_attachment("song.mp3"), AttachmentKind::Audio);
        assert_eq!(classify_attachment("voice.ogg"), AttachmentKind::Audio);
        assert_eq!(classify_attachment("report.pdf"), AttachmentKind::Document);
        assert_eq!(
            classify_attachment("no_extension"),
            AttachmentKind::Document
        );
    }

    // ── directive parsing ──

    #[test]
    fn parses_a_directive_with_caption() {
        let text = "Here is your file.\n\n[[phantom:send_file /home/u/report.pdf | Q3 report]]\n";
        let (cleaned, directives) = parse_send_file_directives(text);
        assert_eq!(cleaned, "Here is your file.");
        assert_eq!(directives.len(), 1);
        assert_eq!(directives[0].path, "/home/u/report.pdf");
        assert_eq!(directives[0].caption.as_deref(), Some("Q3 report"));
    }

    #[test]
    fn parses_a_directive_without_caption() {
        let text = "[[phantom:send_file /home/u/report.pdf]]";
        let (cleaned, directives) = parse_send_file_directives(text);
        assert_eq!(cleaned, "");
        assert_eq!(directives[0].path, "/home/u/report.pdf");
        assert_eq!(directives[0].caption, None);
    }

    #[test]
    fn parses_multiple_directives_in_order() {
        let text =
            "one\n[[phantom:send_file /a.txt]]\ntwo\n[[phantom:send_file /b.txt | b]]\nthree";
        let (cleaned, directives) = parse_send_file_directives(text);
        assert_eq!(cleaned, "one\n\ntwo\n\nthree");
        assert_eq!(directives.len(), 2);
        assert_eq!(directives[0].path, "/a.txt");
        assert_eq!(directives[1].path, "/b.txt");
        assert_eq!(directives[1].caption.as_deref(), Some("b"));
    }

    #[test]
    fn text_without_any_directive_is_unchanged() {
        let text = "Just a normal reply, no attachments.";
        let (cleaned, directives) = parse_send_file_directives(text);
        assert_eq!(cleaned, text);
        assert!(directives.is_empty());
    }

    #[test]
    fn malformed_directive_with_empty_path_is_left_untouched() {
        let text = "oops [[phantom:send_file ]] still here";
        let (cleaned, directives) = parse_send_file_directives(text);
        assert_eq!(cleaned, text);
        assert!(directives.is_empty());
    }

    #[test]
    fn tolerates_extra_whitespace_around_path_and_caption() {
        let text = "[[phantom:send_file   /home/u/x.png   |   caption text  ]]";
        let (_cleaned, directives) = parse_send_file_directives(text);
        assert_eq!(directives[0].path, "/home/u/x.png");
        assert_eq!(directives[0].caption.as_deref(), Some("caption text"));
    }

    // ── path policy ──

    #[test]
    fn accepts_a_regular_file_inside_home() {
        let home = tempfile::tempdir().expect("tempdir");
        let file = home.path().join("report.pdf");
        fs::write(&file, b"hello").expect("write");

        let (resolved, size) =
            validate_send_file_path(&file.to_string_lossy(), home.path()).expect("should pass");
        assert_eq!(size, 5);
        assert_eq!(resolved, fs::canonicalize(&file).unwrap());
    }

    #[test]
    fn rejects_relative_path() {
        let home = tempfile::tempdir().expect("tempdir");
        let err = validate_send_file_path("relative/file.txt", home.path()).unwrap_err();
        assert!(matches!(err, PathPolicyError::NotAbsolute(_)));
    }

    #[test]
    fn rejects_missing_file() {
        let home = tempfile::tempdir().expect("tempdir");
        let missing = home.path().join("nope.txt");
        let err = validate_send_file_path(&missing.to_string_lossy(), home.path()).unwrap_err();
        assert!(matches!(err, PathPolicyError::Unresolvable(_)));
    }

    #[test]
    fn rejects_a_directory() {
        let home = tempfile::tempdir().expect("tempdir");
        let dir = home.path().join("subdir");
        fs::create_dir(&dir).expect("mkdir");
        let err = validate_send_file_path(&dir.to_string_lossy(), home.path()).unwrap_err();
        assert!(matches!(err, PathPolicyError::NotAFile(_)));
    }

    #[test]
    fn rejects_file_outside_home_via_dotdot_traversal() {
        let home = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        let secret = outside.path().join("secret.txt");
        fs::write(&secret, b"nope").expect("write");

        // Escape `home` with `..` components that still canonicalize to a
        // real, existing path outside it.
        let traversal = home
            .path()
            .join("..")
            .join(outside.path().file_name().unwrap())
            .join("secret.txt");
        let err = validate_send_file_path(&traversal.to_string_lossy(), home.path());
        // Either it fails to resolve (relative walk landed nowhere real) or it
        // resolves and is correctly rejected as outside home — both are safe;
        // what must never happen is `Ok`.
        assert!(err.is_err());
    }

    #[test]
    #[cfg(unix)]
    fn rejects_symlink_that_escapes_home() {
        let home = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        let secret = outside.path().join("secret.txt");
        fs::write(&secret, b"nope").expect("write");

        let link = home.path().join("escape.txt");
        symlink(&secret, &link).expect("symlink");

        let err = validate_send_file_path(&link.to_string_lossy(), home.path()).unwrap_err();
        assert!(matches!(err, PathPolicyError::OutsideHome(_)));
    }

    #[test]
    #[cfg(unix)]
    fn accepts_symlink_that_stays_inside_home() {
        let home = tempfile::tempdir().expect("tempdir");
        let real = home.path().join("real.txt");
        fs::write(&real, b"hi").expect("write");
        let link = home.path().join("link.txt");
        symlink(&real, &link).expect("symlink");

        let (resolved, _) =
            validate_send_file_path(&link.to_string_lossy(), home.path()).expect("should pass");
        assert_eq!(resolved, fs::canonicalize(&real).unwrap());
    }

    #[test]
    fn rejects_file_under_ssh() {
        let home = tempfile::tempdir().expect("tempdir");
        let ssh_dir = home.path().join(".ssh");
        fs::create_dir(&ssh_dir).expect("mkdir");
        let file = ssh_dir.join("id_rsa.pub");
        fs::write(&file, b"key").expect("write");

        let err = validate_send_file_path(&file.to_string_lossy(), home.path()).unwrap_err();
        assert!(matches!(err, PathPolicyError::DeniedDirectory(_)));
    }

    #[test]
    fn rejects_file_under_config_codeg() {
        let home = tempfile::tempdir().expect("tempdir");
        let dir = home.path().join(".config").join("codeg");
        fs::create_dir_all(&dir).expect("mkdir");
        let file = dir.join("secrets.db");
        fs::write(&file, b"x").expect("write");

        let err = validate_send_file_path(&file.to_string_lossy(), home.path()).unwrap_err();
        assert!(matches!(err, PathPolicyError::DeniedDirectory(_)));
    }

    #[test]
    fn rejects_dotenv_named_file_even_outside_denied_dirs() {
        let home = tempfile::tempdir().expect("tempdir");
        let file = home.path().join(".env.production");
        fs::write(&file, b"SECRET=1").expect("write");

        let err = validate_send_file_path(&file.to_string_lossy(), home.path()).unwrap_err();
        assert!(matches!(err, PathPolicyError::DeniedName(_)));
    }

    #[test]
    fn rejects_pem_and_key_and_id_underscore_names() {
        let home = tempfile::tempdir().expect("tempdir");
        for name in ["cert.pem", "server.key", "id_ed25519"] {
            let file = home.path().join(name);
            fs::write(&file, b"x").expect("write");
            let err = validate_send_file_path(&file.to_string_lossy(), home.path()).unwrap_err();
            assert!(
                matches!(err, PathPolicyError::DeniedName(_)),
                "{name} should be denied"
            );
        }
    }

    #[test]
    fn rejects_file_over_size_cap() {
        let home = tempfile::tempdir().expect("tempdir");
        let file = home.path().join("big.bin");
        // Sparse file: seek past the cap and write one byte, avoiding an
        // actual 50MB write in the test.
        let f = fs::File::create(&file).expect("create");
        f.set_len(MAX_SEND_FILE_BYTES + 1).expect("set_len");

        let err = validate_send_file_path(&file.to_string_lossy(), home.path()).unwrap_err();
        assert!(matches!(err, PathPolicyError::TooLarge(_, _)));
    }

    #[test]
    fn accepts_file_exactly_at_size_cap() {
        let home = tempfile::tempdir().expect("tempdir");
        let file = home.path().join("exact.bin");
        let f = fs::File::create(&file).expect("create");
        f.set_len(MAX_SEND_FILE_BYTES).expect("set_len");

        let (_resolved, size) =
            validate_send_file_path(&file.to_string_lossy(), home.path()).expect("should pass");
        assert_eq!(size, MAX_SEND_FILE_BYTES);
    }
}
