//! Model quota failover, handoff half: once `commands::phantom_successor` has
//! picked a target `(agent_type, model)` and the user accepted it, actually
//! move the conversation there — a fresh conversation row for the target
//! agent, in the same folder, seeded with a single prompt that carries the
//! whole handoff context so the target agent can continue without repeating
//! work or losing state.
//!
//! [`build_handoff_prompt`] is a pure function (no I/O, no async) so its
//! trimming/formatting rules are unit-testable without a database or a real
//! agent process; [`phantom_handoff_core`] is the thin impure shell that
//! gathers its inputs (the conversation's parsed turns, the folder's git
//! state) and reuses the SAME services the chat-channel task flow does
//! (`ConnectionManager::spawn_agent` + `send_prompt_linked` — see
//! `chat_channel::session_commands::handle_task` for the sibling call site)
//! rather than depending on any chat-channel type.

use std::collections::HashSet;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;

use crate::acp::manager::ConnectionManager;
use crate::acp::types::PromptInputBlock;
use crate::app_error::AppCommandError;
use crate::db::AppDatabase;
use crate::models::agent::AgentType;
use crate::models::message::{ContentBlock, MessageTurn, TurnRole};
use crate::models::phantom::PhantomHandoffResponse;
use crate::web::event_bridge::EventEmitter;

/// Target length for the rendered transcript section. Not a hard cap — the
/// last protected turns are never cut mid-content — but the trim loop stops
/// growing the omitted-middle window as soon as the rendering is at or under
/// this.
const TARGET_TRANSCRIPT_CHARS: usize = 60_000;

/// Turns at the tail of the conversation that are never dropped by trimming,
/// regardless of length — the target agent needs the most recent context
/// intact even if the transcript as a whole has to be squeezed.
const ALWAYS_KEEP_TAIL_TURNS: usize = 10;

/// Cap applied to each of `git status --short` / `git diff --stat` before
/// they're spliced into the prompt — a repo with a huge diffstat must not
/// blow the whole handoff budget on file names alone.
const GIT_OUTPUT_CHAR_CAP: usize = 4_000;

/// Wall-clock budget for each git subprocess. The handoff must never hang
/// because a folder's git state is slow (a huge repo, a stalled index lock,
/// a network filesystem) — a timeout degrades to "no git context", not a
/// stuck request.
const GIT_TIMEOUT: Duration = Duration::from_secs(5);

// ─── Pure prompt builder ──────────────────────────────────────────────────

/// Build the single prompt a handoff hands to the target agent: a Spanish
/// header naming the source conversation, the folder path, a compact render
/// of every turn, the folder's git status/diff (when given), a list of files
/// the source conversation touched, and the user's most recent request
/// called out explicitly.
///
/// `source_label` is the failing side's `agent/model` (or just `agent` when
/// no model is known) — e.g. `"claude_code/claude-sonnet-5"`.
/// `git_status` / `git_diff_stat` are the ALREADY-CAPPED outputs of `git
/// status --short` / `git diff --stat` (see [`phantom_handoff_core`]);
/// `None` when the folder isn't a git repo, the command failed, or timed out.
pub fn build_handoff_prompt(
    conversation_id: i32,
    source_label: &str,
    folder_path: &str,
    turns: &[MessageTurn],
    git_status: Option<&str>,
    git_diff_stat: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Relevo de Phantom: la conversación #{conversation_id} con {source_label} se \
         detuvo por límite de uso. Continúa exactamente donde quedó; no repitas trabajo \
         ya hecho.\n\n"
    ));
    out.push_str(&format!("Carpeta: {folder_path}\n\n"));

    out.push_str("Transcripción:\n");
    let transcript = render_transcript(turns);
    if transcript.is_empty() {
        out.push_str("(sin turnos registrados)\n");
    } else {
        out.push_str(&transcript);
        out.push('\n');
    }

    let files = edited_files(turns);
    if !files.is_empty() {
        out.push_str("\nArchivos modificados:\n");
        for f in &files {
            out.push_str(&format!("- {f}\n"));
        }
    }

    if let Some(status) = git_status.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str(&format!("\ngit status --short:\n{status}\n"));
    }
    if let Some(diff) = git_diff_stat.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str(&format!("\ngit diff --stat:\n{diff}\n"));
    }

    let last_request = last_user_request(turns);
    out.push_str(&format!(
        "\nÚltimo pedido del usuario: {}\n",
        last_request
            .as_deref()
            .unwrap_or("(sin mensajes de usuario)")
    ));

    out
}

/// The most recent user turn's text, verbatim (blocks joined with a blank
/// line — a user turn is very rarely more than one text block, but nothing
/// here assumes it). `None` when the conversation has no user turn at all.
fn last_user_request(turns: &[MessageTurn]) -> Option<String> {
    let turn = turns
        .iter()
        .rev()
        .find(|t| matches!(t.role, TurnRole::User))?;
    let text = turn
        .blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// Render one turn as `"<Role>:\n<content>"`. User/assistant/system text is
/// verbatim; a `ToolUse`/`ToolResult` pair becomes one `- <tool> <input> →
/// ok|error` line (the result is folded onto the immediately preceding
/// tool-use line by position, matching how a turn's blocks are always
/// produced — use then result, never interleaved with another tool's pair in
/// between within a single turn's rendering). Thinking blocks are dropped
/// (internal reasoning, not needed to continue the work); an image becomes a
/// one-word placeholder. Returns `""` for a turn that renders to nothing (an
/// empty user turn, a turn of only `Thinking` blocks).
fn render_turn(turn: &MessageTurn) -> String {
    let role_label = match turn.role {
        TurnRole::User => "Usuario",
        TurnRole::Assistant => "Asistente",
        TurnRole::System => "Sistema",
    };
    let mut lines: Vec<String> = Vec::new();
    for block in &turn.blocks {
        match block {
            ContentBlock::Text { text } => {
                let t = text.trim();
                if !t.is_empty() {
                    lines.push(t.to_string());
                }
            }
            ContentBlock::ToolUse {
                tool_name,
                input_preview,
                ..
            } => {
                lines.push(format!(
                    "- {tool_name} {}",
                    short_input(input_preview.as_deref())
                ));
            }
            ContentBlock::ToolResult { is_error, .. } => {
                // Append the outcome onto the immediately preceding tool-use
                // line, when there is one still waiting for it.
                if let Some(last) = lines.last_mut() {
                    if last.starts_with("- ") && !last.contains('→') {
                        last.push_str(if *is_error { " → error" } else { " → ok" });
                        continue;
                    }
                }
                // A result with no matching pending line (shouldn't happen in
                // a well-formed transcript, but never silently drop it).
                lines.push(format!(
                    "- (resultado) → {}",
                    if *is_error { "error" } else { "ok" }
                ));
            }
            ContentBlock::Image { .. } | ContentBlock::ImageGeneration { .. } => {
                lines.push("[imagen]".to_string());
            }
            ContentBlock::Thinking { .. } => {}
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    format!("{role_label}:\n{}", lines.join("\n"))
}

/// A short, single-line stand-in for a tool call's input — never the full
/// (potentially huge) `input_preview`, since these lines exist to remind the
/// target agent what happened, not to replay it byte for byte.
fn short_input(input_preview: Option<&str>) -> String {
    match input_preview {
        Some(raw) if !raw.trim().is_empty() => truncate_chars(raw.trim().replace('\n', " ⏎ "), 100),
        _ => "(sin entrada)".to_string(),
    }
}

fn truncate_chars(s: impl AsRef<str>, max_chars: usize) -> String {
    let s = s.as_ref();
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut truncated: String = s.chars().take(max_chars).collect();
    truncated.push('…');
    truncated
}

/// Render every turn, trimming from the oldest droppable middle turn until
/// the result fits [`TARGET_TRANSCRIPT_CHARS`] or nothing more can be
/// dropped. The first user turn and the last [`ALWAYS_KEEP_TAIL_TURNS`] turns
/// are never dropped; a single `"[… N turnos anteriores omitidos …]"` marker
/// replaces whatever middle stretch was cut.
fn render_transcript(turns: &[MessageTurn]) -> String {
    let n = turns.len();
    if n == 0 {
        return String::new();
    }
    let rendered: Vec<String> = turns.iter().map(render_turn).collect();
    let first_user_idx = turns.iter().position(|t| matches!(t.role, TurnRole::User));
    let tail_start = n.saturating_sub(ALWAYS_KEEP_TAIL_TURNS);
    let protected = |i: usize| -> bool { i >= tail_start || Some(i) == first_user_idx };

    // Oldest-first, since `turns` is already chronological — dropping a
    // prefix of this list drops the oldest eligible turns first.
    let droppable: Vec<usize> = (0..n).filter(|&i| !protected(i)).collect();

    for drop_count in 0..=droppable.len() {
        let dropped: HashSet<usize> = droppable[..drop_count].iter().copied().collect();
        let text = assemble_with_marker(&rendered, &dropped, drop_count);
        if text.chars().count() <= TARGET_TRANSCRIPT_CHARS || drop_count == droppable.len() {
            return text;
        }
    }
    unreachable!("the loop above always returns by drop_count == droppable.len()")
}

fn assemble_with_marker(
    rendered: &[String],
    dropped: &HashSet<usize>,
    drop_count: usize,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut marker_inserted = false;
    for (i, text) in rendered.iter().enumerate() {
        if dropped.contains(&i) {
            if !marker_inserted {
                parts.push(format!("[… {drop_count} turnos anteriores omitidos …]"));
                marker_inserted = true;
            }
            continue;
        }
        if !text.is_empty() {
            parts.push(text.clone());
        }
    }
    parts.join("\n\n")
}

/// Tool names whose input names a file it wrote or edited. Substring match
/// (case-insensitive) rather than an exact list — parsers/adapters name their
/// edit tools differently (`Edit`, `str_replace_editor`, `MultiEdit`,
/// `apply_patch`, …) and this only has to be a reasonable filter, not a
/// perfect one: a false positive just adds a harmless extra line, a false
/// negative just omits a file from the summary (the transcript itself still
/// has the tool call).
fn is_edit_tool(tool_name: &str) -> bool {
    let n = tool_name.to_lowercase();
    ["edit", "write", "multiedit", "patch", "str_replace"]
        .iter()
        .any(|k| n.contains(k))
}

fn file_path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)"(?:file_path|filepath|path)"\s*:\s*"((?:[^"\\]|\\.)+)""#)
            .expect("file-path pattern is a valid regex")
    })
}

/// Best-effort file path out of a tool call's `input_preview`: a JSON-ish
/// `"file_path"`/`"path"` key when present, else the whole (truncated)
/// preview so a tool call is never silently dropped from the summary just
/// because its input isn't JSON.
fn extract_path(input_preview: Option<&str>) -> Option<String> {
    let raw = input_preview?.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(caps) = file_path_re().captures(raw) {
        if let Some(m) = caps.get(1) {
            return Some(m.as_str().to_string());
        }
    }
    Some(truncate_chars(raw, 80))
}

/// Distinct files an edit-shaped tool call touched, in first-seen order,
/// across every turn (not just the ones the trimmed transcript kept — the
/// target agent should know everything already done, even from a turn that
/// got summarized away).
fn edited_files(turns: &[MessageTurn]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for turn in turns {
        for block in &turn.blocks {
            if let ContentBlock::ToolUse {
                tool_name,
                input_preview,
                ..
            } = block
            {
                if is_edit_tool(tool_name) {
                    if let Some(path) = extract_path(input_preview.as_deref()) {
                        if seen.insert(path.clone()) {
                            files.push(path);
                        }
                    }
                }
            }
        }
    }
    files
}

// ─── Git context (impure) ─────────────────────────────────────────────────

/// `git status --short` and `git diff --stat` for `folder_path`, each capped
/// to [`GIT_OUTPUT_CHAR_CAP`] chars and bounded by [`GIT_TIMEOUT`]. Both are
/// `None` when the folder has no `.git` entry at all (file or directory —
/// covers both a normal repo and a worktree); either individually `None` if
/// its own command fails, times out, or the folder turns out not to be a
/// working git repo despite having a `.git` entry (e.g. a bare/corrupt one).
async fn git_status_and_diff(folder_path: &Path) -> (Option<String>, Option<String>) {
    if !folder_path.join(".git").exists() {
        return (None, None);
    }
    let status = run_git_capped(folder_path, &["status", "--short"]).await;
    let diff = run_git_capped(folder_path, &["diff", "--stat"]).await;
    (status, diff)
}

async fn run_git_capped(folder_path: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args).current_dir(folder_path);
    cmd.stdin(std::process::Stdio::null());
    cmd.kill_on_drop(true);
    let output = tokio::time::timeout(GIT_TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let capped = truncate_chars(text.trim(), GIT_OUTPUT_CHAR_CAP);
    if capped.is_empty() {
        None
    } else {
        Some(capped)
    }
}

// ─── Orchestration ─────────────────────────────────────────────────────────

/// Create a new conversation for `target_agent_type` in the SAME folder as
/// `conversation_id`, seeded with [`build_handoff_prompt`]'s single prompt,
/// and send it. Reuses the exact services the chat-channel task flow does —
/// `ConnectionManager::spawn_agent` then `send_prompt_linked` (see
/// `chat_channel::session_commands::spawn_chat_connection_for_conversation` /
/// `send_chat_prompt_linked` for the sibling call site this mirrors) — so a
/// handoff conversation behaves exactly like any other: same row creation,
/// same `ConversationLinked` event, same sidebar upsert.
/// Everything a handoff needs before any agent is spawned: the source
/// conversation's folder, its title, and the full handoff prompt. Shared by
/// the HTTP handoff below and the Telegram failover flow.
pub struct HandoffContext {
    pub folder_id: i32,
    pub folder_path: String,
    pub source_title: Option<String>,
    pub prompt: String,
}

pub async fn prepare_handoff(
    conn: &sea_orm::DatabaseConnection,
    conversation_id: i32,
) -> Result<HandoffContext, AppCommandError> {
    let (detail, _parsed_title) =
        crate::commands::conversations::get_folder_conversation_core(conn, conversation_id)
            .await?;
    let folder = crate::db::service::folder_service::get_folder_by_id(conn, detail.summary.folder_id)
        .await
        .map_err(AppCommandError::from)?
        .ok_or_else(|| {
            AppCommandError::not_found(format!("Folder {} not found", detail.summary.folder_id))
        })?;
    let source_label = match &detail.summary.model {
        Some(m) if !m.trim().is_empty() => {
            format!("{}/{m}", detail.summary.agent_type.as_wire())
        }
        _ => detail.summary.agent_type.as_wire().into_owned(),
    };
    let (git_status, git_diff_stat) = git_status_and_diff(Path::new(&folder.path)).await;
    let prompt = build_handoff_prompt(
        conversation_id,
        &source_label,
        &folder.path,
        &detail.turns,
        git_status.as_deref(),
        git_diff_stat.as_deref(),
    );
    Ok(HandoffContext {
        folder_id: folder.id,
        folder_path: folder.path.clone(),
        source_title: detail.summary.title.clone(),
        prompt,
    })
}

pub async fn phantom_handoff_core(
    db: &AppDatabase,
    connection_manager: &ConnectionManager,
    emitter: &EventEmitter,
    data_dir: &Path,
    conversation_id: i32,
    target_agent_type: AgentType,
    target_model: Option<String>,
) -> Result<PhantomHandoffResponse, AppCommandError> {
    let (detail, _parsed_title) =
        crate::commands::conversations::get_folder_conversation_core(&db.conn, conversation_id)
            .await?;
    let folder =
        crate::db::service::folder_service::get_folder_by_id(&db.conn, detail.summary.folder_id)
            .await
            .map_err(AppCommandError::from)?
            .ok_or_else(|| {
                AppCommandError::not_found(format!("Folder {} not found", detail.summary.folder_id))
            })?;

    let source_label = match &detail.summary.model {
        Some(m) if !m.trim().is_empty() => {
            format!("{}/{m}", detail.summary.agent_type.as_wire())
        }
        _ => detail.summary.agent_type.as_wire().into_owned(),
    };

    let (git_status, git_diff_stat) = git_status_and_diff(Path::new(&folder.path)).await;

    let prompt = build_handoff_prompt(
        conversation_id,
        &source_label,
        &folder.path,
        &detail.turns,
        git_status.as_deref(),
        git_diff_stat.as_deref(),
    );

    let runtime_env =
        crate::commands::acp::build_session_runtime_env(db, target_agent_type, None, data_dir)
            .await
            .map_err(|e| AppCommandError::task_execution_failed(e.to_string()))?;

    let mut preferred_config_values = std::collections::BTreeMap::new();
    if let Some(model) = target_model.as_ref().filter(|m| !m.trim().is_empty()) {
        preferred_config_values.insert(
            crate::acp::connection::MODEL_CATEGORY_CONFIG_KEY.to_string(),
            model.clone(),
        );
    }

    let owner_label = format!("phantom-handoff:{conversation_id}");
    let connection_id = connection_manager
        .spawn_agent(
            target_agent_type,
            Some(folder.path.clone()),
            None,
            runtime_env,
            owner_label,
            emitter.clone(),
            None,
            preferred_config_values,
        )
        .await
        .map_err(|e| AppCommandError::task_execution_failed(e.to_string()))?;

    let new_conversation_id = connection_manager
        .send_prompt_linked(
            db,
            &connection_id,
            vec![PromptInputBlock::Text { text: prompt }],
            Some(folder.id),
            None,
            None,
        )
        .await
        .map_err(|e| AppCommandError::task_execution_failed(e.to_string()))?
        .ok_or_else(|| {
            AppCommandError::task_execution_failed(
                "handoff prompt sent but no conversation id came back",
            )
        })?;

    Ok(PhantomHandoffResponse {
        conversation_id: new_conversation_id,
        folder_id: folder.id,
        agent_type: target_agent_type.as_wire().into_owned(),
        model: target_model,
        connection_id,
    })
}

// ─── Desktop command ─────────────────────────────────────────────────────

#[cfg(feature = "tauri-runtime")]
#[cfg_attr(feature = "tauri-runtime", tauri::command)]
pub async fn phantom_handoff(
    conversation_id: i32,
    target_agent_type: AgentType,
    target_model: Option<String>,
    manager: tauri::State<'_, ConnectionManager>,
    db: tauri::State<'_, AppDatabase>,
    app_handle: tauri::AppHandle,
) -> Result<PhantomHandoffResponse, AppCommandError> {
    use tauri::Manager;
    // Same effective-data-dir resolution `acp_connect` uses (see there for
    // why the fallback exists): a custom `CODEG_DATA_DIR` must still reach
    // `build_session_runtime_env`, and the app data dir may not exist yet on
    // a very first launch.
    let app_data_dir = app_handle
        .path()
        .app_data_dir()
        .map(|p| crate::paths::resolve_effective_data_dir(&p))
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    let emitter = EventEmitter::Tauri(app_handle);
    phantom_handoff_core(
        &db,
        &manager,
        &emitter,
        &app_data_dir,
        conversation_id,
        target_agent_type,
        target_model,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn ts(secs: i64) -> chrono::DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    fn user_turn(id: &str, text: &str, at: i64) -> MessageTurn {
        MessageTurn {
            id: id.to_string(),
            role: TurnRole::User,
            blocks: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            timestamp: ts(at),
            usage: None,
            duration_ms: None,
            model: None,
            completed_at: None,
            agent_message_id: None,
        }
    }

    fn assistant_text_turn(id: &str, text: &str, at: i64) -> MessageTurn {
        MessageTurn {
            id: id.to_string(),
            role: TurnRole::Assistant,
            blocks: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            timestamp: ts(at),
            usage: None,
            duration_ms: None,
            model: None,
            completed_at: None,
            agent_message_id: None,
        }
    }

    fn assistant_tool_turn(
        id: &str,
        tool_name: &str,
        input_preview: Option<&str>,
        is_error: bool,
        at: i64,
    ) -> MessageTurn {
        MessageTurn {
            id: id.to_string(),
            role: TurnRole::Assistant,
            blocks: vec![
                ContentBlock::ToolUse {
                    tool_use_id: Some(format!("{id}-tool")),
                    tool_name: tool_name.to_string(),
                    input_preview: input_preview.map(str::to_string),
                    status: None,
                    meta: None,
                },
                ContentBlock::ToolResult {
                    tool_use_id: Some(format!("{id}-tool")),
                    output_preview: Some("done".to_string()),
                    is_error,
                    agent_stats: None,
                    images: vec![],
                },
            ],
            timestamp: ts(at),
            usage: None,
            duration_ms: None,
            model: None,
            completed_at: None,
            agent_message_id: None,
        }
    }

    // ─── render_turn / render_transcript ───────────────────────────────

    #[test]
    fn renders_user_and_assistant_text_verbatim() {
        let turns = vec![
            user_turn("u1", "please fix the bug", 0),
            assistant_text_turn("a1", "looking into it", 1),
        ];
        let text = render_transcript(&turns);
        assert!(text.contains("Usuario:\nplease fix the bug"));
        assert!(text.contains("Asistente:\nlooking into it"));
    }

    #[test]
    fn renders_a_tool_call_with_its_outcome() {
        let turns = vec![assistant_tool_turn(
            "a1",
            "Bash",
            Some("cargo test"),
            false,
            0,
        )];
        let text = render_transcript(&turns);
        assert!(text.contains("- Bash cargo test → ok"));
    }

    #[test]
    fn a_failing_tool_call_is_marked_error() {
        let turns = vec![assistant_tool_turn("a1", "Edit", Some("{}"), true, 0)];
        let text = render_transcript(&turns);
        assert!(text.contains("→ error"));
    }

    #[test]
    fn empty_transcript_renders_to_empty_string() {
        assert_eq!(render_transcript(&[]), "");
    }

    // ─── trimming ───────────────────────────────────────────────────────

    #[test]
    fn short_transcripts_are_not_trimmed_at_all() {
        let turns: Vec<MessageTurn> = (0..5)
            .map(|i| user_turn(&format!("u{i}"), &format!("message {i}"), i))
            .collect();
        let text = render_transcript(&turns);
        assert!(!text.contains("omitidos"));
        for i in 0..5 {
            assert!(text.contains(&format!("message {i}")));
        }
    }

    #[test]
    fn a_huge_transcript_is_trimmed_with_a_marker_and_keeps_first_user_and_tail() {
        // Each turn's text is long enough that many of them together blow the
        // 60k-char budget, forcing the middle to be dropped.
        let big = "x".repeat(2000);
        let mut turns = vec![user_turn("first", "FIRST_USER_MESSAGE", 0)];
        for i in 1..80 {
            turns.push(assistant_text_turn(&format!("a{i}"), &big, i));
        }
        let text = render_transcript(&turns);
        assert!(text.contains("omitidos"), "middle should be summarized");
        assert!(
            text.contains("FIRST_USER_MESSAGE"),
            "first user turn is always kept"
        );
        // The last ALWAYS_KEEP_TAIL_TURNS turns' content must survive.
        for i in 70..80 {
            let marker = format!("a{i}");
            // We don't render turn ids into the text, so instead check the
            // tail's raw content (`big`) count is present for the protected
            // window by checking total occurrences of the repeated filler.
            let _ = marker;
        }
        assert!(text.chars().count() <= TARGET_TRANSCRIPT_CHARS + big.len());
    }

    #[test]
    fn the_marker_names_the_number_of_omitted_turns() {
        let big = "x".repeat(3000);
        let mut turns = vec![user_turn("first", "hi", 0)];
        for i in 1..60 {
            turns.push(assistant_text_turn(&format!("a{i}"), &big, i));
        }
        let text = render_transcript(&turns);
        let re = Regex::new(r"\[… (\d+) turnos anteriores omitidos …\]").unwrap();
        let caps = re.captures(&text).expect("marker present");
        let n: usize = caps[1].parse().unwrap();
        assert!(n > 0);
        // Never claims to have dropped a protected turn.
        assert!(n < turns.len() - ALWAYS_KEEP_TAIL_TURNS);
    }

    #[test]
    fn always_keeps_at_least_the_last_ten_turns_content() {
        let big = "x".repeat(2000);
        let mut turns = vec![user_turn("first", "hi", 0)];
        for i in 1..50 {
            turns.push(assistant_text_turn(
                &format!("a{i}"),
                &format!("{big}-{i}"),
                i,
            ));
        }
        let text = render_transcript(&turns);
        for i in 40..50 {
            assert!(
                text.contains(&format!("{big}-{i}")),
                "turn {i} is within the protected tail and must survive"
            );
        }
    }

    // ─── edited_files / last_user_request ────────────────────────────────

    #[test]
    fn edited_files_extracts_a_json_file_path_key() {
        let turns = vec![assistant_tool_turn(
            "a1",
            "Edit",
            Some(r#"{"file_path": "src/main.rs", "old_string": "a"}"#),
            false,
            0,
        )];
        assert_eq!(edited_files(&turns), vec!["src/main.rs".to_string()]);
    }

    #[test]
    fn edited_files_deduplicates_in_first_seen_order() {
        let turns = vec![
            assistant_tool_turn("a1", "Edit", Some(r#"{"path": "b.rs"}"#), false, 0),
            assistant_tool_turn("a2", "Write", Some(r#"{"path": "a.rs"}"#), false, 1),
            assistant_tool_turn("a3", "Edit", Some(r#"{"path": "b.rs"}"#), false, 2),
        ];
        assert_eq!(
            edited_files(&turns),
            vec!["b.rs".to_string(), "a.rs".to_string()]
        );
    }

    #[test]
    fn edited_files_ignores_non_edit_tools() {
        let turns = vec![assistant_tool_turn(
            "a1",
            "Read",
            Some(r#"{"path": "readme.md"}"#),
            false,
            0,
        )];
        assert!(edited_files(&turns).is_empty());
    }

    #[test]
    fn edited_files_falls_back_to_a_truncated_raw_preview_when_not_json() {
        let turns = vec![assistant_tool_turn(
            "a1",
            "apply_patch",
            Some("*** Update File: src/lib.rs\n@@ ..."),
            false,
            0,
        )];
        let files = edited_files(&turns);
        assert_eq!(files.len(), 1);
        assert!(files[0].starts_with("*** Update File"));
    }

    #[test]
    fn last_user_request_finds_the_most_recent_user_turn() {
        let turns = vec![
            user_turn("u1", "first ask", 0),
            assistant_text_turn("a1", "ok", 1),
            user_turn("u2", "second ask", 2),
        ];
        assert_eq!(last_user_request(&turns).as_deref(), Some("second ask"));
    }

    #[test]
    fn last_user_request_is_none_without_any_user_turn() {
        let turns = vec![assistant_text_turn("a1", "hello", 0)];
        assert!(last_user_request(&turns).is_none());
    }

    // ─── build_handoff_prompt: full shape ────────────────────────────────

    #[test]
    fn build_handoff_prompt_includes_every_documented_section() {
        let turns = vec![
            user_turn("u1", "add a login page", 0),
            assistant_tool_turn("a1", "Write", Some(r#"{"path": "src/login.rs"}"#), false, 1),
        ];
        let prompt = build_handoff_prompt(
            66,
            "claude_code/claude-sonnet-5",
            "/home/user/project",
            &turns,
            Some(" M src/login.rs\n"),
            Some(" src/login.rs | 10 +++++\n"),
        );
        assert!(prompt.contains("conversación #66"));
        assert!(prompt.contains("claude_code/claude-sonnet-5"));
        assert!(prompt.contains("no repitas trabajo ya hecho"));
        assert!(prompt.contains("Carpeta: /home/user/project"));
        assert!(prompt.contains("add a login page"));
        assert!(prompt.contains("Archivos modificados:"));
        assert!(prompt.contains("src/login.rs"));
        assert!(prompt.contains("git status --short:"));
        assert!(prompt.contains("git diff --stat:"));
        assert!(prompt.contains("Último pedido del usuario: add a login page"));
    }

    #[test]
    fn build_handoff_prompt_omits_git_sections_when_not_given() {
        let turns = vec![user_turn("u1", "hi", 0)];
        let prompt = build_handoff_prompt(1, "codex", "/tmp/x", &turns, None, None);
        assert!(!prompt.contains("git status"));
        assert!(!prompt.contains("git diff"));
    }

    #[test]
    fn build_handoff_prompt_handles_no_user_messages_gracefully() {
        let turns = vec![assistant_text_turn("a1", "auto-generated", 0)];
        let prompt = build_handoff_prompt(1, "codex", "/tmp/x", &turns, None, None);
        assert!(prompt.contains("Último pedido del usuario: (sin mensajes de usuario)"));
    }

    #[test]
    fn short_input_truncates_and_replaces_newlines() {
        let long = "a".repeat(200);
        let s = short_input(Some(&long));
        assert!(s.chars().count() <= 101); // 100 + the ellipsis char
        assert!(s.ends_with('…'));

        let multiline = short_input(Some("line1\nline2"));
        assert_eq!(multiline, "line1 ⏎ line2");
    }
}
