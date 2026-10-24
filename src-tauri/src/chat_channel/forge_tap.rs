//! FORGE tap (Phantom only, behind `PHANTOM_FORGE_TAP=1`).
//!
//! Before a plain Telegram message goes to the agent, the local `nexus forge`
//! CLI gets a look at it: exit 0 means FORGE owned it and its stdout is the
//! reply; anything else (exit 3 "not mine", errors, timeout, empty stdout)
//! sends the message on to the agent exactly as without the tap. Photos, which
//! the Telegram backend otherwise drops, go to `nexus forge foto` by file id.

use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use tokio::sync::Mutex;

use super::agenda_tap::nexus_bin;
use super::question_buttons;
use super::session_bridge::SessionBridge;
use super::types::IncomingCommand;

pub const FLAG: &str = "PHANTOM_FORGE_TAP";
const TEXT_TIMEOUT: Duration = Duration::from_secs(15);
/// Local vision runs on the photo, so it gets much longer than text.
const PHOTO_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_TEXT_CHARS: usize = 4000;

/// What the dispatcher does with the message after the tap.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// FORGE handled it: send this on the same chat, don't forward.
    Reply(String),
    /// Carry on exactly as without the tap.
    Forward,
}

pub fn enabled() -> bool {
    flag_on(std::env::var_os(FLAG).as_deref())
}

fn flag_on(value: Option<&OsStr>) -> bool {
    value == Some(OsStr::new("1"))
}

/// Telegram ids of the message the command came from.
#[derive(Debug, PartialEq, Eq)]
pub struct MessageMeta {
    pub chat_id: String,
    pub message_id: i64,
    pub date: i64,
}

pub fn message_meta(metadata: &serde_json::Value) -> Option<MessageMeta> {
    let message = metadata.get("message")?;
    let chat_id = match message.pointer("/chat/id")? {
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        _ => return None,
    };
    Some(MessageMeta {
        chat_id,
        message_id: message.get("message_id")?.as_i64()?,
        date: message.get("date")?.as_i64()?,
    })
}

#[derive(Debug, PartialEq, Eq)]
pub struct Photo {
    pub file_id: String,
    pub caption: Option<String>,
}

/// Largest size of `message.photo` plus the caption, if any.
pub fn extract_photo(message: &serde_json::Value) -> Option<Photo> {
    let size = |p: &serde_json::Value| {
        let field = |k| p.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
        (field("file_size"), field("width") * field("height"))
    };
    let largest = message
        .get("photo")?
        .as_array()?
        .iter()
        .filter(|p| p.get("file_id").and_then(|v| v.as_str()).is_some())
        .max_by_key(|p| size(p))?;
    Some(Photo {
        file_id: largest.get("file_id")?.as_str()?.to_string(),
        caption: message
            .get("caption")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_string),
    })
}

fn capped(text: &str) -> String {
    text.chars().take(MAX_TEXT_CHARS).collect()
}

fn id_args(meta: &MessageMeta) -> [String; 6] {
    [
        "--chat-id".to_string(),
        meta.chat_id.clone(),
        "--message-id".to_string(),
        meta.message_id.to_string(),
        "--date".to_string(),
        meta.date.to_string(),
    ]
}

/// The text goes last, after `--`: «--help» or «-h» from Telegram is text for
/// FORGE, never an option of the CLI.
fn ingest_args(text: &str, meta: &MessageMeta) -> Vec<String> {
    let mut args = vec!["forge".to_string(), "ingest".to_string()];
    args.extend(id_args(meta));
    args.extend(["--".to_string(), capped(text)]);
    args
}

fn foto_args(photo: &Photo, meta: &MessageMeta) -> Vec<String> {
    let mut args = vec![
        "forge".to_string(),
        "foto".to_string(),
        "--file-id".to_string(),
        photo.file_id.clone(),
    ];
    args.extend(id_args(meta));
    if let Some(caption) = &photo.caption {
        // `--pie=…` in one argument: a caption starting with "-" stays the caption.
        args.push(format!("--pie={}", capped(caption)));
    }
    args
}

/// Set on a photo's caption once FORGE turned the photo down, so it goes on
/// to the agent untouched instead of being tapped a second time.
const FORWARDED_KEY: &str = "phantom_forge_tap_forwarded";

fn already_forwarded(cmd: &IncomingCommand) -> bool {
    cmd.metadata.get(FORWARDED_KEY).is_some()
}

/// The caption of a photo FORGE didn't take, as a plain-text command that
/// skips the tap.
pub fn caption_forward(cmd: &IncomingCommand) -> IncomingCommand {
    let mut forward = IncomingCommand {
        channel_id: cmd.channel_id,
        sender_id: cmd.sender_id.clone(),
        command_text: cmd.command_text.clone(),
        callback_data: cmd.callback_data.clone(),
        target: cmd.target.clone(),
        metadata: cmd.metadata.clone(),
        voice_reply_lang: cmd.voice_reply_lang.clone(),
    };
    if !forward.metadata.is_object() {
        forward.metadata = serde_json::json!({});
    }
    forward.metadata[FORWARDED_KEY] = serde_json::Value::Bool(true);
    forward
}

/// Plain text that would go to the agent: no button tap, no command, and not
/// the Telegram general topic (plain text there never reaches an agent).
fn is_tappable_text(cmd: &IncomingCommand, prefix: &str) -> bool {
    let text = cmd.command_text.trim();
    cmd.callback_data.is_none()
        && !already_forwarded(cmd)
        && !text.is_empty()
        && !text.starts_with('/')
        && text.strip_prefix(prefix).is_none()
        && !cmd.target.is_telegram_general_topic()
}

/// How a CLI run ended: its exit code and trimmed stdout, or why it didn't.
enum Run {
    Exited(Option<i32>, String),
    Timeout,
    SpawnError(String),
    BadOutput,
}

async fn run_raw(bin: &Path, args: &[String], timeout: Duration) -> Run {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => match String::from_utf8(out.stdout) {
            Ok(stdout) => Run::Exited(out.status.code(), stdout.trim().to_string()),
            Err(_) => Run::BadOutput,
        },
        Ok(Err(e)) => Run::SpawnError(format!("spawn error ({})", e.kind())),
        Err(_) => Run::Timeout,
    }
}

/// `Ok(stdout)` only for exit 0 with non-empty output; `Err(reason)` otherwise.
async fn run_cli(bin: &Path, args: &[String], timeout: Duration) -> Result<String, String> {
    match run_raw(bin, args, timeout).await {
        Run::Exited(Some(0), reply) if !reply.is_empty() => Ok(reply),
        Run::Exited(Some(0), _) => Err("empty stdout".to_string()),
        Run::Exited(Some(code), _) => Err(format!("exit {code}")),
        Run::Exited(None, _) => Err("killed by signal".to_string()),
        Run::Timeout => Err("timeout".to_string()),
        Run::SpawnError(why) => Err(why),
        Run::BadOutput => Err("stdout is not UTF-8".to_string()),
    }
}

/// Said when local vision didn't finish in time on a photo without caption:
/// the photo has nowhere else to go, so Julian hears about it.
pub const PHOTO_TOO_SLOW: &str =
    "📷 No alcancé a leer la foto a tiempo; no registré nada. Mándala otra vez o escríbeme la comida.";

/// Said when FORGE couldn't take a photo without caption (not installed,
/// crashed, no answer): a photo is never dropped in silence.
pub const PHOTO_FAILED: &str =
    "📷 No pude procesar la foto (FORGE no respondió); no registré nada. Mándala otra vez más tarde o escríbeme la comida.";

/// Whether Phantom is waiting on this chat to answer a permission or question.
async fn awaiting_answer(bridge: &Mutex<SessionBridge>, cmd: &IncomingCommand) -> bool {
    let guard = bridge.lock().await;
    let session = if cmd.target.is_telegram_forum_topic() {
        guard.find_by_target(&cmd.target)
    } else {
        guard.find_by_sender(cmd.channel_id, &cmd.sender_id)
    };
    session.is_some_and(|s| {
        s.permission_pending.is_some() || question_buttons::has_pending(&s.connection_id)
    })
}

/// The photo FORGE should see, if the flag is on and this is one.
pub fn photo(cmd: &IncomingCommand) -> Option<Photo> {
    if !enabled() {
        return None;
    }
    photo_in(cmd)
}

fn photo_in(cmd: &IncomingCommand) -> Option<Photo> {
    if cmd.callback_data.is_some() || already_forwarded(cmd) {
        return None;
    }
    extract_photo(cmd.metadata.get("message")?)
}

/// The text hook: `Reply` or `Forward`. With the flag off it is `Forward`
/// without spawning anything.
pub async fn intercept_text(
    cmd: &IncomingCommand,
    prefix: &str,
    bridge: &Mutex<SessionBridge>,
) -> Outcome {
    if !enabled() {
        return Outcome::Forward;
    }
    let awaiting = awaiting_answer(bridge, cmd).await;
    text_with(&nexus_bin(), TEXT_TIMEOUT, cmd, prefix, awaiting).await
}

/// The photo hook (slow: local vision). `Forward` means "send the caption on".
pub async fn intercept_photo(cmd: &IncomingCommand, photo: &Photo) -> Outcome {
    photo_with(&nexus_bin(), PHOTO_TIMEOUT, cmd, photo).await
}

async fn text_with(
    bin: &Path,
    timeout: Duration,
    cmd: &IncomingCommand,
    prefix: &str,
    awaiting: bool,
) -> Outcome {
    if awaiting || !is_tappable_text(cmd, prefix) {
        return Outcome::Forward;
    }
    let result = match message_meta(&cmd.metadata) {
        Some(meta) => run_cli(bin, &ingest_args(cmd.command_text.trim(), &meta), timeout).await,
        None => Err("no message ids".to_string()),
    };
    match result {
        Ok(reply) => {
            tracing::info!("[ForgeTap] ingest channel={}: handled", cmd.channel_id);
            Outcome::Reply(reply)
        }
        Err(why) => {
            tracing::info!(
                "[ForgeTap] ingest channel={}: {why}, forwarded",
                cmd.channel_id
            );
            Outcome::Forward
        }
    }
}

async fn photo_with(
    bin: &Path,
    timeout: Duration,
    cmd: &IncomingCommand,
    photo: &Photo,
) -> Outcome {
    let Some(meta) = message_meta(&cmd.metadata) else {
        return if photo.caption.is_some() {
            Outcome::Forward
        } else {
            Outcome::Reply(PHOTO_FAILED.to_string())
        };
    };
    match run_raw(bin, &foto_args(photo, &meta), timeout).await {
        Run::Exited(Some(0), reply) if !reply.is_empty() => {
            tracing::info!("[ForgeTap] foto channel={}: handled", cmd.channel_id);
            Outcome::Reply(reply)
        }
        _ if photo.caption.is_some() => {
            tracing::info!("[ForgeTap] foto channel={}: caption forwarded", cmd.channel_id);
            Outcome::Forward
        }
        // No caption: nothing would reach the agent, so FORGE's own words
        // ("📷 No vi comida…") or a timeout notice go back instead of silence.
        Run::Exited(Some(3), said) if !said.is_empty() => Outcome::Reply(said),
        Run::Timeout => Outcome::Reply(PHOTO_TOO_SLOW.to_string()),
        _ => {
            tracing::info!(
                "[ForgeTap] foto channel={}: FORGE unavailable, no caption",
                cmd.channel_id
            );
            Outcome::Reply(PHOTO_FAILED.to_string())
        }
    }
}

/// Serializes the tests that set `PHANTOM_FORGE_TAP` / `NEXUS_BIN`.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: Mutex<()> = Mutex::const_new(());

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::chat_channel::types::ChannelMessageTarget;
    use serde_json::json;
    use std::path::PathBuf;

    const SHORT: Duration = Duration::from_secs(5);

    /// A fake `nexus`: records its argv (one per line) next to itself, prints
    /// `stdout` and exits with `code`. Written by a child shell so no write fd
    /// of ours is open when another test thread forks (ETXTBSY).
    pub(crate) fn fake_nexus(dir: &Path, stdout: &str, code: i32) -> PathBuf {
        let bin = dir.join("nexus");
        let args = dir.join("args");
        let body = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s' '{}'\nexit {}\n",
            args.display(),
            stdout,
            code
        );
        let ok = std::process::Command::new("sh")
            .arg("-c")
            .arg("printf '%s' \"$1\" > \"$2\" && chmod +x \"$2\"")
            .arg("sh")
            .arg(body)
            .arg(&bin)
            .status()
            .expect("write fake nexus")
            .success();
        assert!(ok);
        bin
    }

    pub(crate) fn recorded_args(dir: &Path) -> Option<Vec<String>> {
        std::fs::read_to_string(dir.join("args"))
            .ok()
            .map(|s| s.lines().map(str::to_string).collect())
    }

    fn update(message: serde_json::Value) -> serde_json::Value {
        json!({ "update_id": 1, "message": message })
    }

    fn text_cmd(text: &str) -> IncomingCommand {
        IncomingCommand {
            channel_id: 7,
            sender_id: "42".to_string(),
            command_text: text.to_string(),
            callback_data: None,
            target: ChannelMessageTarget::channel(7),
            metadata: update(json!({
                "message_id": 555,
                "date": 1760389200,
                "chat": { "id": 42, "type": "private" },
                "from": { "id": 42 },
                "text": text,
            })),
            voice_reply_lang: None,
        }
    }

    fn photo_cmd(caption: Option<&str>) -> IncomingCommand {
        let mut message = json!({
            "message_id": 556,
            "date": 1760389300,
            "chat": { "id": 42, "type": "private" },
            "from": { "id": 42 },
            "photo": [
                { "file_id": "small", "width": 90, "height": 90, "file_size": 1000 },
                { "file_id": "big", "width": 1280, "height": 960, "file_size": 90000 },
                { "file_id": "mid", "width": 320, "height": 240, "file_size": 9000 },
            ],
        });
        if let Some(c) = caption {
            message["caption"] = json!(c);
        }
        IncomingCommand {
            command_text: caption.unwrap_or_default().to_string(),
            metadata: update(message),
            ..text_cmd("")
        }
    }

    // ── flag ──

    #[test]
    fn flag_is_on_only_for_exactly_one() {
        assert!(flag_on(Some(OsStr::new("1"))));
        for off in [None, Some(""), Some("0"), Some("true"), Some(" 1")] {
            assert!(!flag_on(off.map(OsStr::new)), "{off:?}");
        }
    }

    /// Touches `PHANTOM_FORGE_TAP` / `NEXUS_BIN`; holds `TEST_ENV_LOCK`.
    #[tokio::test]
    async fn flag_off_forwards_without_running_nexus_and_on_runs_it() {
        let _env = TEST_ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_nexus(dir.path(), "anotado", 0);
        std::env::set_var("NEXUS_BIN", &bin);
        let bridge = Mutex::new(SessionBridge::new());
        let cmd = text_cmd("press banca 80x5");

        std::env::remove_var(FLAG);
        assert_eq!(intercept_text(&cmd, "/", &bridge).await, Outcome::Forward);
        assert_eq!(photo(&photo_cmd(Some("cena"))), None);
        assert_eq!(
            recorded_args(dir.path()),
            None,
            "nexus ran with the flag off"
        );

        std::env::set_var(FLAG, "1");
        let on = intercept_text(&cmd, "/", &bridge).await;
        let on_photo = photo(&photo_cmd(None)).map(|p| p.file_id);
        std::env::remove_var(FLAG);
        std::env::remove_var("NEXUS_BIN");
        assert_eq!(on, Outcome::Reply("anotado".to_string()));
        assert_eq!(on_photo.as_deref(), Some("big"));
    }

    // ── parsing ──

    #[test]
    fn message_meta_reads_chat_message_and_date() {
        let cmd = text_cmd("hola");
        assert_eq!(
            message_meta(&cmd.metadata),
            Some(MessageMeta {
                chat_id: "42".to_string(),
                message_id: 555,
                date: 1760389200,
            })
        );
        assert_eq!(message_meta(&json!({ "callback_query": {} })), None);
    }

    #[test]
    fn extract_photo_takes_the_largest_size_and_caption() {
        let cmd = photo_cmd(Some("cena"));
        let message = cmd.metadata.get("message").unwrap();
        assert_eq!(
            extract_photo(message),
            Some(Photo {
                file_id: "big".to_string(),
                caption: Some("cena".to_string()),
            })
        );
    }

    #[test]
    fn extract_photo_without_caption_or_photo() {
        let cmd = photo_cmd(None);
        let message = cmd.metadata.get("message").unwrap();
        assert_eq!(extract_photo(message).unwrap().caption, None);
        assert_eq!(extract_photo(&json!({ "text": "hola" })), None);
        assert_eq!(extract_photo(&json!({ "photo": [] })), None);
    }

    // ── argument building ──

    #[test]
    fn ingest_args_carry_text_and_ids() {
        let meta = message_meta(&text_cmd("x").metadata).unwrap();
        assert_eq!(
            ingest_args("press \"banca\"; rm -rf ~", &meta),
            [
                "forge",
                "ingest",
                "--chat-id",
                "42",
                "--message-id",
                "555",
                "--date",
                "1760389200",
                "--",
                "press \"banca\"; rm -rf ~",
            ]
        );
    }

    #[test]
    fn ingest_args_cap_the_text() {
        let meta = message_meta(&text_cmd("x").metadata).unwrap();
        let long = "ñ".repeat(MAX_TEXT_CHARS + 50);
        let args = ingest_args(&long, &meta);
        assert_eq!(args.last().unwrap().chars().count(), MAX_TEXT_CHARS);
    }

    #[test]
    fn text_that_looks_like_an_option_stays_text() {
        let meta = message_meta(&text_cmd("x").metadata).unwrap();
        for text in ["--help", "-h", "--he"] {
            let args = ingest_args(text, &meta);
            assert_eq!(args[args.len() - 2..], ["--", text]);
        }
        let photo = Photo {
            file_id: "big".to_string(),
            caption: Some("-200 g de arroz".to_string()),
        };
        assert_eq!(foto_args(&photo, &meta).last().unwrap(), "--pie=-200 g de arroz");
    }

    #[test]
    fn foto_args_with_and_without_caption() {
        let meta = message_meta(&photo_cmd(None).metadata).unwrap();
        let photo = Photo {
            file_id: "big".to_string(),
            caption: None,
        };
        assert_eq!(
            foto_args(&photo, &meta),
            [
                "forge",
                "foto",
                "--file-id",
                "big",
                "--chat-id",
                "42",
                "--message-id",
                "556",
                "--date",
                "1760389300",
            ]
        );
        let photo = Photo {
            caption: Some("cena".to_string()),
            ..photo
        };
        let args = foto_args(&photo, &meta);
        assert_eq!(args.last().unwrap(), "--pie=cena");
    }

    // ── which messages are tapped ──

    #[test]
    fn plain_text_is_tappable_commands_and_taps_are_not() {
        assert!(is_tappable_text(&text_cmd("press banca 80x5"), "/"));
        assert!(!is_tappable_text(&text_cmd("/task hola"), "/"));
        assert!(!is_tappable_text(&text_cmd("/status"), "!"));
        assert!(!is_tappable_text(&text_cmd("!status"), "!"));
        assert!(!is_tappable_text(&text_cmd("   "), "/"));
        let tap = IncomingCommand {
            callback_data: Some("nav:menu".to_string()),
            ..text_cmd("nav:menu")
        };
        assert!(!is_tappable_text(&tap, "/"));
        let general = IncomingCommand {
            target: ChannelMessageTarget::telegram_general(7, "42"),
            ..text_cmd("hola")
        };
        assert!(!is_tappable_text(&general, "/"));
    }

    #[tokio::test]
    async fn awaiting_answer_is_false_without_a_session() {
        let bridge = Mutex::new(SessionBridge::new());
        assert!(!awaiting_answer(&bridge, &text_cmd("hola")).await);
    }

    #[tokio::test]
    async fn awaiting_answer_while_a_permission_is_pending() {
        use crate::chat_channel::session_bridge::{ActiveSession, PendingPermission};
        let cmd = text_cmd("sí");
        let session = |pending: bool| ActiveSession {
            channel_id: cmd.channel_id,
            sender_id: cmd.sender_id.clone(),
            target: cmd.target.clone(),
            conversation_id: 1,
            connection_id: "forge-tap-conn".to_string(),
            agent_type: crate::models::agent::AgentType::Codex,
            content_buffer: String::new(),
            tool_calls: Vec::new(),
            tool_call_inputs: std::collections::HashMap::new(),
            delegation_rendered: std::collections::HashSet::new(),
            last_flushed: std::time::Instant::now(),
            pending_prompt: None,
            permission_pending: pending.then(|| PendingPermission {
                request_id: "r1".to_string(),
                tool_description: "Bash ls".to_string(),
                options: Vec::new(),
                sent_message_id: None,
            }),
            pending_voice_reply_lang: None,
        };
        let bridge = Mutex::new(SessionBridge::new());
        bridge
            .lock()
            .await
            .register("forge-tap-conn".to_string(), session(false));
        assert!(!awaiting_answer(&bridge, &cmd).await);
        bridge
            .lock()
            .await
            .register("forge-tap-conn".to_string(), session(true));
        assert!(awaiting_answer(&bridge, &cmd).await);
    }

    // ── text: CLI outcomes ──

    #[tokio::test]
    async fn exit_zero_replies_and_is_not_forwarded() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_nexus(dir.path(), "\n  ✅ Banca 80x5 anotado \n", 0);
        let out = text_with(&bin, SHORT, &text_cmd("press banca 80x5"), "/", false).await;
        assert_eq!(out, Outcome::Reply("✅ Banca 80x5 anotado".to_string()));
        let args = recorded_args(dir.path()).unwrap();
        assert_eq!(args[..2], ["forge", "ingest"]);
        assert_eq!(args[args.len() - 2..], ["--", "press banca 80x5"]);
    }

    #[tokio::test]
    async fn exit_three_is_forwarded() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_nexus(dir.path(), "no es mio", 3);
        let out = text_with(&bin, SHORT, &text_cmd("hola"), "/", false).await;
        assert_eq!(out, Outcome::Forward);
        assert!(recorded_args(dir.path()).is_some(), "nexus should have run");
    }

    #[tokio::test]
    async fn other_exit_code_is_forwarded() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_nexus(dir.path(), "boom", 1);
        let out = text_with(&bin, SHORT, &text_cmd("hola"), "/", false).await;
        assert_eq!(out, Outcome::Forward);
    }

    #[tokio::test]
    async fn exit_zero_with_empty_stdout_is_forwarded() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_nexus(dir.path(), " \n ", 0);
        let out = text_with(&bin, SHORT, &text_cmd("hola"), "/", false).await;
        assert_eq!(out, Outcome::Forward);
    }

    #[tokio::test]
    async fn timeout_is_forwarded() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("nexus");
        let ok = std::process::Command::new("sh")
            .arg("-c")
            .arg("printf '#!/bin/sh\\nsleep 10\\necho tarde\\n' > \"$1\" && chmod +x \"$1\"")
            .arg("sh")
            .arg(&bin)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let started = std::time::Instant::now();
        let out = text_with(
            &bin,
            Duration::from_millis(300),
            &text_cmd("hola"),
            "/",
            false,
        )
        .await;
        assert_eq!(out, Outcome::Forward);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn spawn_error_is_forwarded() {
        let out = text_with(
            Path::new("/nonexistent/forge-tap/nexus"),
            SHORT,
            &text_cmd("hola"),
            "/",
            false,
        )
        .await;
        assert_eq!(out, Outcome::Forward);
    }

    #[tokio::test]
    async fn commands_taps_and_pending_answers_never_run_nexus() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_nexus(dir.path(), "anotado", 0);
        let tap = IncomingCommand {
            callback_data: Some("ag:ok:abc".to_string()),
            ..text_cmd("ag:ok:abc")
        };
        for (cmd, awaiting) in [
            (text_cmd("/task hola"), false),
            (tap, false),
            (text_cmd("sí, dale"), true),
        ] {
            assert_eq!(
                text_with(&bin, SHORT, &cmd, "/", awaiting).await,
                Outcome::Forward
            );
        }
        assert_eq!(recorded_args(dir.path()), None);
    }

    #[tokio::test]
    async fn transcribed_voice_is_tapped_like_text() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_nexus(dir.path(), "anotado", 0);
        let voice = IncomingCommand {
            voice_reply_lang: Some("es".to_string()),
            ..text_cmd("peso setenta y ocho")
        };
        let out = text_with(&bin, SHORT, &voice, "/", false).await;
        assert_eq!(out, Outcome::Reply("anotado".to_string()));
    }

    // ── photos ──

    async fn photo_out(bin: &Path, cmd: &IncomingCommand) -> Outcome {
        let photo = photo_in(cmd).expect("a photo message");
        photo_with(bin, SHORT, cmd, &photo).await
    }

    #[test]
    fn photo_in_skips_taps_and_messages_without_a_photo() {
        assert_eq!(photo_in(&text_cmd("hola")), None);
        let tap = IncomingCommand {
            callback_data: Some("nav:menu".to_string()),
            ..photo_cmd(None)
        };
        assert_eq!(photo_in(&tap), None);
    }

    #[test]
    fn a_forwarded_caption_is_neither_photo_nor_tappable_text() {
        let forward = caption_forward(&photo_cmd(Some("mira esto")));
        assert_eq!(forward.command_text, "mira esto");
        assert_eq!(photo_in(&forward), None);
        assert!(!is_tappable_text(&forward, "/"));
        let odd = IncomingCommand {
            metadata: json!(null),
            ..text_cmd("hola")
        };
        assert!(!is_tappable_text(&caption_forward(&odd), "/"));
    }

    #[tokio::test]
    async fn photo_timeout_without_caption_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("nexus");
        let ok = std::process::Command::new("sh")
            .arg("-c")
            .arg("printf '#!/bin/sh\\nsleep 10\\n' > \"$1\" && chmod +x \"$1\"")
            .arg("sh")
            .arg(&bin)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let cmd = photo_cmd(None);
        let photo = photo_in(&cmd).unwrap();
        let out = photo_with(&bin, Duration::from_millis(300), &cmd, &photo).await;
        assert_eq!(out, Outcome::Reply(PHOTO_TOO_SLOW.to_string()));
    }

    #[tokio::test]
    async fn photo_exit_zero_replies() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_nexus(dir.path(), "🍽 Cena registrada", 0);
        let out = photo_out(&bin, &photo_cmd(Some("cena"))).await;
        assert_eq!(out, Outcome::Reply("🍽 Cena registrada".to_string()));
        let args = recorded_args(dir.path()).unwrap();
        assert_eq!(args[..4], ["forge", "foto", "--file-id", "big"]);
        assert_eq!(args.last().unwrap(), "--pie=cena");
    }

    #[tokio::test]
    async fn photo_with_caption_not_taken_forwards_the_caption() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_nexus(dir.path(), "", 3);
        let out = photo_out(&bin, &photo_cmd(Some("mira esto"))).await;
        assert_eq!(out, Outcome::Forward);
    }

    #[tokio::test]
    async fn photo_without_caption_not_taken_tells_why() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_nexus(dir.path(), "📷 No vi comida en la foto; no registré nada.", 3);
        let out = photo_out(&bin, &photo_cmd(None)).await;
        assert_eq!(
            out,
            Outcome::Reply("📷 No vi comida en la foto; no registré nada.".to_string())
        );
    }

    #[tokio::test]
    async fn photo_without_caption_and_without_forge_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_nexus(dir.path(), "", 3);
        let out = photo_out(&bin, &photo_cmd(None)).await;
        assert_eq!(out, Outcome::Reply(PHOTO_FAILED.to_string()));
        let out = photo_out(Path::new("/nonexistent/forge-tap/nexus"), &photo_cmd(None)).await;
        assert_eq!(out, Outcome::Reply(PHOTO_FAILED.to_string()));
    }

    #[tokio::test]
    async fn invalid_utf8_from_forge_forwards_the_text() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("nexus");
        std::fs::write(&bin, "#!/bin/sh\nprintf '\\377\\376'\nexit 0\n").unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let out = text_with(&bin, Duration::from_secs(5), &text_cmd("hola"), "/", false).await;
        assert_eq!(out, Outcome::Forward);
    }
}
