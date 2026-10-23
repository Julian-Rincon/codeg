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
const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_TEXT_CHARS: usize = 4000;

/// What the dispatcher does with the message after the tap.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// FORGE handled it: send this on the same chat, don't forward.
    Reply(String),
    /// Carry on exactly as without the tap.
    Forward,
    /// A captionless photo FORGE didn't take: nothing to forward.
    Drop,
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

fn ingest_args(text: &str, meta: &MessageMeta) -> Vec<String> {
    let mut args = vec!["forge".to_string(), "ingest".to_string(), capped(text)];
    args.extend(id_args(meta));
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
        args.extend(["--pie".to_string(), capped(caption)]);
    }
    args
}

/// Plain text that would go to the agent: no button tap, no command, and not
/// the Telegram general topic (plain text there never reaches an agent).
fn is_tappable_text(cmd: &IncomingCommand, prefix: &str) -> bool {
    let text = cmd.command_text.trim();
    cmd.callback_data.is_none()
        && !text.is_empty()
        && !text.starts_with('/')
        && text.strip_prefix(prefix).is_none()
        && !cmd.target.is_telegram_general_topic()
}

/// `Ok(stdout)` only for exit 0 with non-empty output; `Err(reason)` otherwise.
async fn run_cli(bin: &Path, args: &[String], timeout: Duration) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(format!("spawn error ({})", e.kind())),
        Err(_) => return Err("timeout".to_string()),
    };
    match out.status.code() {
        Some(0) => {
            let reply = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if reply.is_empty() {
                Err("empty stdout".to_string())
            } else {
                Ok(reply)
            }
        }
        Some(code) => Err(format!("exit {code}")),
        None => Err("killed by signal".to_string()),
    }
}

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

/// The dispatcher hook. With the flag off this is `Forward` without spawning.
pub async fn intercept(
    cmd: &IncomingCommand,
    prefix: &str,
    bridge: &Mutex<SessionBridge>,
) -> Outcome {
    if !enabled() {
        return Outcome::Forward;
    }
    let awaiting = awaiting_answer(bridge, cmd).await;
    intercept_with(&nexus_bin(), TIMEOUT, cmd, prefix, awaiting).await
}

async fn intercept_with(
    bin: &Path,
    timeout: Duration,
    cmd: &IncomingCommand,
    prefix: &str,
    awaiting: bool,
) -> Outcome {
    let photo = cmd
        .metadata
        .get("message")
        .filter(|_| cmd.callback_data.is_none())
        .and_then(extract_photo);
    if let Some(photo) = photo {
        let has_caption = photo.caption.is_some();
        let result = match message_meta(&cmd.metadata) {
            Some(meta) => run_cli(bin, &foto_args(&photo, &meta), timeout).await,
            None => Err("no message ids".to_string()),
        };
        return match result {
            Ok(reply) => {
                tracing::info!("[ForgeTap] foto channel={}: handled", cmd.channel_id);
                Outcome::Reply(reply)
            }
            Err(why) if has_caption => {
                tracing::info!(
                    "[ForgeTap] foto channel={}: {why}, caption forwarded",
                    cmd.channel_id
                );
                Outcome::Forward
            }
            Err(why) => {
                tracing::info!("[ForgeTap] foto channel={}: {why}, dropped", cmd.channel_id);
                Outcome::Drop
            }
        };
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_channel::types::ChannelMessageTarget;
    use serde_json::json;
    use std::path::PathBuf;

    const SHORT: Duration = Duration::from_secs(5);

    /// A fake `nexus`: records its argv (one per line) next to itself, prints
    /// `stdout` and exits with `code`. Written by a child shell so no write fd
    /// of ours is open when another test thread forks (ETXTBSY).
    fn fake_nexus(dir: &Path, stdout: &str, code: i32) -> PathBuf {
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

    fn recorded_args(dir: &Path) -> Option<Vec<String>> {
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

    /// The only test that touches `PHANTOM_FORGE_TAP` / `NEXUS_BIN`.
    #[tokio::test]
    async fn flag_off_forwards_without_running_nexus_and_on_runs_it() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_nexus(dir.path(), "anotado", 0);
        std::env::set_var("NEXUS_BIN", &bin);
        let bridge = Mutex::new(SessionBridge::new());
        let cmd = text_cmd("press banca 80x5");

        std::env::remove_var(FLAG);
        assert_eq!(intercept(&cmd, "/", &bridge).await, Outcome::Forward);
        assert_eq!(
            recorded_args(dir.path()),
            None,
            "nexus ran with the flag off"
        );

        std::env::set_var(FLAG, "1");
        let on = intercept(&cmd, "/", &bridge).await;
        std::env::remove_var(FLAG);
        std::env::remove_var("NEXUS_BIN");
        assert_eq!(on, Outcome::Reply("anotado".to_string()));
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
                "press \"banca\"; rm -rf ~",
                "--chat-id",
                "42",
                "--message-id",
                "555",
                "--date",
                "1760389200",
            ]
        );
    }

    #[test]
    fn ingest_args_cap_the_text() {
        let meta = message_meta(&text_cmd("x").metadata).unwrap();
        let long = "ñ".repeat(MAX_TEXT_CHARS + 50);
        assert_eq!(ingest_args(&long, &meta)[2].chars().count(), MAX_TEXT_CHARS);
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
        assert_eq!(&args[args.len() - 2..], ["--pie", "cena"]);
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

    // ── text: CLI outcomes ──

    #[tokio::test]
    async fn exit_zero_replies_and_is_not_forwarded() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_nexus(dir.path(), "\n  ✅ Banca 80x5 anotado \n", 0);
        let out = intercept_with(&bin, SHORT, &text_cmd("press banca 80x5"), "/", false).await;
        assert_eq!(out, Outcome::Reply("✅ Banca 80x5 anotado".to_string()));
        assert_eq!(
            recorded_args(dir.path()).unwrap()[..3],
            ["forge", "ingest", "press banca 80x5"]
        );
    }

    #[tokio::test]
    async fn exit_three_is_forwarded() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_nexus(dir.path(), "no es mio", 3);
        let out = intercept_with(&bin, SHORT, &text_cmd("hola"), "/", false).await;
        assert_eq!(out, Outcome::Forward);
        assert!(recorded_args(dir.path()).is_some(), "nexus should have run");
    }

    #[tokio::test]
    async fn other_exit_code_is_forwarded() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_nexus(dir.path(), "boom", 1);
        let out = intercept_with(&bin, SHORT, &text_cmd("hola"), "/", false).await;
        assert_eq!(out, Outcome::Forward);
    }

    #[tokio::test]
    async fn exit_zero_with_empty_stdout_is_forwarded() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_nexus(dir.path(), " \n ", 0);
        let out = intercept_with(&bin, SHORT, &text_cmd("hola"), "/", false).await;
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
        let out = intercept_with(
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
        let out = intercept_with(
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
                intercept_with(&bin, SHORT, &cmd, "/", awaiting).await,
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
        let out = intercept_with(&bin, SHORT, &voice, "/", false).await;
        assert_eq!(out, Outcome::Reply("anotado".to_string()));
    }

    // ── photos ──

    #[tokio::test]
    async fn photo_exit_zero_replies() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_nexus(dir.path(), "🍽 Cena registrada", 0);
        let out = intercept_with(&bin, SHORT, &photo_cmd(Some("cena")), "/", false).await;
        assert_eq!(out, Outcome::Reply("🍽 Cena registrada".to_string()));
        let args = recorded_args(dir.path()).unwrap();
        assert_eq!(args[..4], ["forge", "foto", "--file-id", "big"]);
        assert_eq!(args[args.len() - 2..], ["--pie", "cena"]);
    }

    #[tokio::test]
    async fn photo_with_caption_not_taken_forwards_the_caption() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_nexus(dir.path(), "", 3);
        let out = intercept_with(&bin, SHORT, &photo_cmd(Some("mira esto")), "/", false).await;
        assert_eq!(out, Outcome::Forward);
    }

    #[tokio::test]
    async fn photo_without_caption_not_taken_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_nexus(dir.path(), "", 3);
        let out = intercept_with(&bin, SHORT, &photo_cmd(None), "/", false).await;
        assert_eq!(out, Outcome::Drop);
        let out = intercept_with(
            Path::new("/nonexistent/forge-tap/nexus"),
            SHORT,
            &photo_cmd(None),
            "/",
            false,
        )
        .await;
        assert_eq!(out, Outcome::Drop);
    }
}
