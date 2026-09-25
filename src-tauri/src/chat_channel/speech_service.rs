//! Client for the local speech service (`/stt`, `/tts`) that backs Telegram
//! voice notes, plus the ffmpeg pipe that turns a synthesized WAV reply into
//! the OGG/Opus format Telegram's `sendVoice` requires.
//!
//! The service is optional and may simply not be running; every call here
//! returns a typed error rather than panicking so callers can degrade to a
//! plain text reply (see `SpeechServiceError::Unavailable`).

use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::AsyncWriteExt;

/// Default address of the local speech service.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:3091";
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap on the text handed to `/tts`. Telegram voice notes are meant to be
/// short; a longer reply is summarized by truncating at a sentence boundary
/// rather than sent verbatim.
const TTS_MAX_CHARS: usize = 1200;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SttResponse {
    pub text: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum SpeechServiceError {
    #[error("speech service is not running at {0}")]
    Unavailable(String),
    #[error("speech service request failed: {0}")]
    Request(String),
    #[error("speech service returned status {0}: {1}")]
    Status(u16, String),
    #[error("failed to spawn ffmpeg: {0}")]
    FfmpegSpawn(String),
    #[error("ffmpeg conversion failed: {0}")]
    Ffmpeg(String),
    #[error("ffmpeg conversion timed out")]
    FfmpegTimeout,
}

#[derive(Clone)]
pub struct SpeechServiceClient {
    base_url: String,
    /// Bearer token — the server's own `CODEG_TOKEN`, reused rather than
    /// minted separately since the speech service trusts the same value.
    /// Never logged (see `redact_token` convention used elsewhere in this
    /// module tree).
    token: Option<String>,
    client: reqwest::Client,
}

impl SpeechServiceClient {
    /// Build a client from environment: `CODEG_SPEECH_SERVICE_URL` (falls
    /// back to [`DEFAULT_BASE_URL`]) and `CODEG_TOKEN` for bearer auth.
    pub fn from_env() -> Self {
        let base_url = std::env::var("CODEG_SPEECH_SERVICE_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let token = std::env::var("CODEG_TOKEN")
            .ok()
            .filter(|s| !s.trim().is_empty());
        Self::new(base_url, token)
    }

    pub fn new(base_url: impl Into<String>, token: Option<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token,
            client: reqwest::Client::builder()
                .connect_timeout(HTTP_CONNECT_TIMEOUT)
                .timeout(HTTP_TIMEOUT)
                .build()
                .unwrap_or_default(),
        }
    }

    fn authed(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => builder.bearer_auth(t),
            None => builder,
        }
    }

    /// Transcribe raw, ffmpeg-decodable audio bytes (any container/codec,
    /// including Telegram's OGG/Opus voice notes) via `POST /stt`.
    pub async fn transcribe(&self, audio: Vec<u8>) -> Result<SttResponse, SpeechServiceError> {
        let url = format!("{}/stt", self.base_url.trim_end_matches('/'));
        let resp = self
            .authed(self.client.post(&url).body(audio))
            .send()
            .await
            .map_err(|e| classify_transport_error(e, &self.base_url))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(SpeechServiceError::Status(status, body));
        }

        resp.json::<SttResponse>()
            .await
            .map_err(|e| SpeechServiceError::Request(e.to_string()))
    }

    /// Synthesize `text` (already prepared by [`prepare_tts_text`]) in `lang`
    /// (`"es"` or `"en"`) via `POST /tts`, returning raw WAV bytes.
    pub async fn synthesize(&self, text: &str, lang: &str) -> Result<Vec<u8>, SpeechServiceError> {
        let url = format!("{}/tts", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({ "text": text, "lang": lang });
        let resp = self
            .authed(self.client.post(&url).json(&body))
            .send()
            .await
            .map_err(|e| classify_transport_error(e, &self.base_url))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(SpeechServiceError::Status(status, body));
        }

        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| SpeechServiceError::Request(e.to_string()))
    }
}

fn classify_transport_error(err: reqwest::Error, base_url: &str) -> SpeechServiceError {
    if err.is_connect() {
        SpeechServiceError::Unavailable(base_url.to_string())
    } else {
        SpeechServiceError::Request(err.to_string())
    }
}

/// Pick the reply language from the STT response's `language` field: a value
/// starting with `en` (case-insensitive, so `en-US`/`en_GB` count too) maps
/// to English; everything else — including a missing or unrecognized value —
/// defaults to Spanish, per spec.
pub fn pick_reply_language(stt_language: Option<&str>) -> &'static str {
    match stt_language {
        Some(lang) if lang.trim().to_ascii_lowercase().starts_with("en") => "en",
        _ => "es",
    }
}

/// Prepare an agent's reply text for `/tts`: collapse whitespace/newlines
/// into single spaces, then cap at [`TTS_MAX_CHARS`], cutting at the last
/// sentence boundary (`.`, `!`, `?`) at or before the cap so the spoken reply
/// doesn't trail off mid-word. Falls back to a hard cut if no boundary falls
/// in a reasonable range (avoids a single early "e.g." turning a long reply
/// into three words).
pub fn prepare_tts_text(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= TTS_MAX_CHARS {
        return collapsed;
    }

    let truncated: String = collapsed.chars().take(TTS_MAX_CHARS).collect();
    let boundary = ['.', '!', '?']
        .iter()
        .filter_map(|c| truncated.rfind(*c))
        .max();

    match boundary {
        // Only trust a boundary past the first quarter of the window —
        // otherwise one early abbreviation period would gut the reply.
        Some(idx) if idx > TTS_MAX_CHARS / 4 => truncated[..=idx].trim_end().to_string(),
        _ => truncated.trim_end().to_string(),
    }
}

/// ffmpeg arguments that convert a complete WAV file on stdin into OGG/Opus
/// on stdout. Split out as a pure function so the invocation is unit
/// testable without ffmpeg actually being installed.
pub fn ffmpeg_wav_to_ogg_opus_args() -> Vec<&'static str> {
    vec![
        "-hide_banner",
        "-loglevel",
        "error",
        "-i",
        "pipe:0",
        "-f",
        "ogg",
        "-c:a",
        "libopus",
        "pipe:1",
    ]
}

/// Convert `wav_bytes` (a complete WAV file) into OGG/Opus by piping it
/// through ffmpeg, with a 30s timeout. Writing stdin happens on a separate
/// task so a large input can't deadlock against ffmpeg's stdout buffer
/// filling up before all of it is written.
pub async fn wav_to_ogg_opus(wav_bytes: Vec<u8>) -> Result<Vec<u8>, SpeechServiceError> {
    let mut child = crate::process::tokio_command("ffmpeg")
        .args(ffmpeg_wav_to_ogg_opus_args())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| SpeechServiceError::FfmpegSpawn(e.to_string()))?;

    let mut stdin = child.stdin.take().expect("stdin was piped");
    let write_task = tokio::spawn(async move {
        let _ = stdin.write_all(&wav_bytes).await;
        // `stdin` drops here, closing the pipe and signaling EOF to ffmpeg.
    });

    let wait = child.wait_with_output();
    let output = tokio::time::timeout(FFMPEG_TIMEOUT, wait)
        .await
        .map_err(|_| SpeechServiceError::FfmpegTimeout)?
        .map_err(|e| SpeechServiceError::Ffmpeg(e.to_string()))?;
    let _ = write_task.await;

    if !output.status.success() {
        return Err(SpeechServiceError::Ffmpeg(format!(
            "ffmpeg exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── language pick ──

    #[test]
    fn picks_english_from_plain_and_regioned_tags() {
        assert_eq!(pick_reply_language(Some("en")), "en");
        assert_eq!(pick_reply_language(Some("en-US")), "en");
        assert_eq!(pick_reply_language(Some("EN_gb")), "en");
    }

    #[test]
    fn defaults_to_spanish_for_everything_else() {
        assert_eq!(pick_reply_language(Some("es")), "es");
        assert_eq!(pick_reply_language(Some("fr")), "es");
        assert_eq!(pick_reply_language(None), "es");
        assert_eq!(pick_reply_language(Some("")), "es");
    }

    // ── tts text prep ──

    #[test]
    fn collapses_whitespace_and_newlines() {
        assert_eq!(prepare_tts_text("hola   \n\n  mundo"), "hola mundo");
    }

    #[test]
    fn short_text_is_returned_as_is() {
        let text = "Listo, ya lo hice.";
        assert_eq!(prepare_tts_text(text), text);
    }

    #[test]
    fn long_text_is_cut_at_a_sentence_boundary() {
        let sentence = "Esta es una oracion de prueba bastante larga para forzar el corte. ";
        let long = sentence.repeat(30);
        let result = prepare_tts_text(&long);
        assert!(result.chars().count() <= TTS_MAX_CHARS);
        assert!(
            result.ends_with('.'),
            "should end at a sentence boundary: {result:?}"
        );
    }

    #[test]
    fn falls_back_to_hard_cut_when_no_good_boundary_exists() {
        let long = "a".repeat(TTS_MAX_CHARS * 2);
        let result = prepare_tts_text(&long);
        assert_eq!(result.chars().count(), TTS_MAX_CHARS);
    }

    // ── ffmpeg invocation shape ──

    #[test]
    fn ffmpeg_args_read_stdin_and_write_ogg_opus_to_stdout() {
        let args = ffmpeg_wav_to_ogg_opus_args();
        assert_eq!(
            args,
            vec![
                "-hide_banner",
                "-loglevel",
                "error",
                "-i",
                "pipe:0",
                "-f",
                "ogg",
                "-c:a",
                "libopus",
                "pipe:1",
            ]
        );
    }

    // ── STT response shape ──

    #[test]
    fn stt_response_deserializes_with_optional_fields_absent() {
        let json = serde_json::json!({ "text": "hola" });
        let parsed: SttResponse = serde_json::from_value(json).expect("parses");
        assert_eq!(parsed.text, "hola");
        assert_eq!(parsed.language, None);
        assert_eq!(parsed.duration_ms, None);
    }

    #[test]
    fn stt_response_deserializes_with_all_fields() {
        let json = serde_json::json!({ "text": "hi", "language": "en", "duration_ms": 1200 });
        let parsed: SttResponse = serde_json::from_value(json).expect("parses");
        assert_eq!(parsed.text, "hi");
        assert_eq!(parsed.language.as_deref(), Some("en"));
        assert_eq!(parsed.duration_ms, Some(1200));
    }

    // ── client construction ──

    #[test]
    fn new_stores_base_url_and_token_without_making_a_request() {
        let client = SpeechServiceClient::new(DEFAULT_BASE_URL, Some("secret".to_string()));
        assert_eq!(client.base_url, DEFAULT_BASE_URL);
        assert_eq!(client.token.as_deref(), Some("secret"));
    }
}
