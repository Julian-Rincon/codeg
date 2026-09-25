use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex, OnceCell};

use crate::chat_channel::error::ChatChannelError;
use crate::chat_channel::i18n::{self, Lang};
use crate::chat_channel::traits::ChatChannelBackend;
use crate::chat_channel::types::*;

pub struct TelegramBackend {
    bot_token: String,
    chat_id: String,
    topic_mode: bool,
    client: reqwest::Client,
    status: Arc<Mutex<ChannelConnectionStatus>>,
    channel_id: i32,
    shutdown_tx: Arc<Mutex<Option<tokio::sync::watch::Sender<bool>>>>,
    /// Cached canonical numeric chat id, resolved from an `@username`
    /// `chat_id` via `getChat`. Empty for numeric configs (never populated).
    resolved_chat_id: OnceCell<String>,
}

impl TelegramBackend {
    pub fn new(channel_id: i32, bot_token: String, chat_id: String, topic_mode: bool) -> Self {
        Self {
            bot_token,
            chat_id,
            topic_mode,
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(60))
                .build()
                .unwrap_or_default(),
            status: Arc::new(Mutex::new(ChannelConnectionStatus::Disconnected)),
            channel_id,
            shutdown_tx: Arc::new(Mutex::new(None)),
            resolved_chat_id: OnceCell::new(),
        }
    }

    fn api_url(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{}", self.bot_token, method)
    }

    /// Canonical **numeric** chat id used for topic-binding keys.
    ///
    /// Telegram delivers inbound updates keyed by the numeric `/chat/id`, but a
    /// channel may be configured with an `@username`. Topic bindings are written
    /// from `create_thread`'s returned target and later looked up against inbound
    /// targets, so both sides must agree on the id form or the lookup misses
    /// (leaving follow-ups "unbound" and spawning a duplicate agent). Resolve
    /// `@username` → numeric once via `getChat` and cache it; numeric ids pass
    /// through untouched. On failure the cell stays empty so the next call
    /// retries.
    async fn canonical_chat_id(&self) -> Result<String, ChatChannelError> {
        if !self.chat_id.trim_start().starts_with('@') {
            return Ok(self.chat_id.clone());
        }
        self.resolved_chat_id
            .get_or_try_init(|| async {
                let body = serde_json::json!({ "chat_id": self.chat_id });
                let resp = self
                    .client
                    .post(self.api_url("getChat"))
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| {
                        ChatChannelError::SendFailed(redact_token(e.to_string(), &self.bot_token))
                    })?;
                let result: serde_json::Value = resp.json().await.map_err(|e| {
                    ChatChannelError::SendFailed(redact_token(e.to_string(), &self.bot_token))
                })?;
                result
                    .pointer("/result/id")
                    .and_then(|v| v.as_i64())
                    .map(|id| id.to_string())
                    .ok_or_else(|| {
                        ChatChannelError::SendFailed(
                            "Telegram getChat returned no numeric chat id".to_string(),
                        )
                    })
            })
            .await
            .cloned()
    }

    async fn send_text(
        &self,
        text: &str,
        parse_mode: Option<&str>,
        target: Option<&ChannelMessageTarget>,
    ) -> Result<SentMessageId, ChatChannelError> {
        self.send_text_with_reply_markup(text, parse_mode, target, None)
            .await
    }

    async fn send_text_with_reply_markup(
        &self,
        text: &str,
        parse_mode: Option<&str>,
        target: Option<&ChannelMessageTarget>,
        reply_markup: Option<serde_json::Value>,
    ) -> Result<SentMessageId, ChatChannelError> {
        let body =
            telegram_send_message_body(&self.chat_id, text, parse_mode, target, reply_markup)?;

        let resp = self
            .client
            .post(self.api_url("sendMessage"))
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                ChatChannelError::SendFailed(redact_token(e.to_string(), &self.bot_token))
            })?;

        let result: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| {
                ChatChannelError::SendFailed(redact_token(e.to_string(), &self.bot_token))
            })?;

        if result.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let desc = result
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(ChatChannelError::SendFailed(desc.to_string()));
        }

        let message_id = result
            .pointer("/result/message_id")
            .and_then(|v| v.as_i64())
            .map(|id| id.to_string())
            .unwrap_or_default();

        Ok(SentMessageId(message_id))
    }

    async fn send_interactive_message_with_target(
        &self,
        message: &InteractiveMessage,
        target: Option<&ChannelMessageTarget>,
    ) -> Result<SentMessageId, ChatChannelError> {
        let reply_markup = telegram_inline_keyboard(message);
        let markdown_text = format_telegram_markdown(&message.base);
        let result = self
            .send_text_with_reply_markup(
                &markdown_text,
                Some("MarkdownV2"),
                target,
                reply_markup.clone(),
            )
            .await;

        match result {
            Ok(id) => Ok(id),
            Err(e) => {
                tracing::warn!(
                    "[Telegram] MarkdownV2 interactive send failed: {e}, retrying as plain text"
                );
                self.send_text_with_reply_markup(
                    &message.base.to_plain_text(),
                    None,
                    target,
                    reply_markup,
                )
                .await
            }
        }
    }

    async fn send_rich_message_with_target(
        &self,
        message: &RichMessage,
        target: Option<&ChannelMessageTarget>,
    ) -> Result<SentMessageId, ChatChannelError> {
        let markdown_text = format_telegram_markdown(message);
        let result = self
            .send_text(&markdown_text, Some("MarkdownV2"), target)
            .await;

        match result {
            Ok(id) => Ok(id),
            Err(e) => {
                // MarkdownV2 failed — fall back to plain text, preserving topic target.
                tracing::warn!("[Telegram] MarkdownV2 send failed: {e}, retrying as plain text");
                let plain_text = message.to_plain_text();
                self.send_text(&plain_text, None, target).await
            }
        }
    }

    /// Shared multipart upload for `sendDocument` / `sendPhoto` /
    /// `sendAudio` / `sendVoice`: `field_name` is the Bot API's file field
    /// for that method (`document`, `photo`, `audio`, `voice`) and `method`
    /// is the endpoint name.
    #[allow(clippy::too_many_arguments)]
    async fn upload_media(
        &self,
        method: &str,
        field_name: &str,
        target: &ChannelMessageTarget,
        bytes: Vec<u8>,
        filename: &str,
        mime: Option<&str>,
        caption: Option<&str>,
    ) -> Result<SentMessageId, ChatChannelError> {
        let chat_id = target
            .chat_id
            .clone()
            .unwrap_or_else(|| self.chat_id.clone());

        let mut part = reqwest::multipart::Part::bytes(bytes).file_name(filename.to_string());
        if let Some(mime) = mime {
            part = part
                .mime_str(mime)
                .map_err(|e| ChatChannelError::SendFailed(format!("invalid mime type: {e}")))?;
        }

        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id)
            .part(field_name.to_string(), part);
        if target.is_telegram_forum_topic() {
            let thread_id = target
                .thread_key
                .as_deref()
                .and_then(|s| s.parse::<i64>().ok())
                .ok_or_else(|| {
                    ChatChannelError::SendFailed(
                        "invalid Telegram message_thread_id target".to_string(),
                    )
                })?;
            form = form.text("message_thread_id", thread_id.to_string());
        }
        if let Some(caption) = caption {
            form = form.text("caption", caption.to_string());
        }

        let resp = self
            .client
            .post(self.api_url(method))
            .multipart(form)
            .send()
            .await
            .map_err(|e| {
                ChatChannelError::SendFailed(redact_token(e.to_string(), &self.bot_token))
            })?;

        let result: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| {
                ChatChannelError::SendFailed(redact_token(e.to_string(), &self.bot_token))
            })?;

        if result.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let desc = result
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(ChatChannelError::SendFailed(desc.to_string()));
        }

        let message_id = result
            .pointer("/result/message_id")
            .and_then(|v| v.as_i64())
            .map(|id| id.to_string())
            .unwrap_or_default();
        Ok(SentMessageId(message_id))
    }
}

#[async_trait]
impl ChatChannelBackend for TelegramBackend {
    fn channel_type(&self) -> ChannelType {
        ChannelType::Telegram
    }

    async fn start(
        &self,
        command_tx: mpsc::Sender<IncomingCommand>,
    ) -> Result<(), ChatChannelError> {
        *self.status.lock().await = ChannelConnectionStatus::Connecting;

        // Verify bot token and extract bot username for group @mention filtering
        let resp = self
            .client
            .get(self.api_url("getMe"))
            .send()
            .await
            .map_err(|e| {
                ChatChannelError::ConnectionFailed(redact_token(e.to_string(), &self.bot_token))
            })?;

        let me_body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| {
                ChatChannelError::ConnectionFailed(redact_token(e.to_string(), &self.bot_token))
            })?;

        if me_body.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            *self.status.lock().await = ChannelConnectionStatus::Error;
            return Err(ChatChannelError::AuthenticationFailed(
                "Invalid bot token".to_string(),
            ));
        }

        let bot_username = me_body
            .pointer("/result/username")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();

        *self.status.lock().await = ChannelConnectionStatus::Connected;

        // Start long-polling loop
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        *self.shutdown_tx.lock().await = Some(shutdown_tx);

        let client = self.client.clone();
        let bot_token = self.bot_token.clone();
        let channel_id = self.channel_id;
        // Resolve `@username` → numeric once so inbound targets/matching use the
        // same id form the topic bindings are stored under. Falls back to the
        // raw value on failure (`telegram_message_chat_matches` still matches via
        // /chat/username).
        let configured_chat_id = self
            .canonical_chat_id()
            .await
            .unwrap_or_else(|_| self.chat_id.clone());
        let topic_mode = self.topic_mode;
        let status = self.status.clone();
        let speech_client = crate::chat_channel::speech_service::SpeechServiceClient::from_env();

        tokio::spawn(async move {
            let mut offset: i64 = 0;
            loop {
                if *shutdown_rx.borrow() {
                    break;
                }

                let url = format!("https://api.telegram.org/bot{}/getUpdates", bot_token);
                let body = serde_json::json!({
                    "timeout": 30,
                    "offset": offset,
                    "allowed_updates": ["message", "callback_query"],
                });

                let result = tokio::select! {
                    r = client.post(&url).json(&body).send() => r,
                    _ = shutdown_rx.changed() => break,
                };

                match result {
                    Ok(resp) => {
                        // Recover from error state after successful poll
                        {
                            let mut s = status.lock().await;
                            if *s == ChannelConnectionStatus::Error {
                                *s = ChannelConnectionStatus::Connected;
                            }
                        }

                        if let Ok(body) = resp.json::<serde_json::Value>().await {
                            if let Some(updates) = body.get("result").and_then(|r| r.as_array()) {
                                if !updates.is_empty() {
                                    tracing::info!("[Telegram] got {} update(s)", updates.len());
                                }
                                for update in updates {
                                    if let Some(uid) =
                                        update.get("update_id").and_then(|u| u.as_i64())
                                    {
                                        offset = uid + 1;
                                    }
                                    if let Some(message) = update.get("message") {
                                        if !telegram_message_chat_matches(
                                            message,
                                            &configured_chat_id,
                                        ) {
                                            tracing::debug!(
                                                "[Telegram] skipped message from unconfigured chat"
                                            );
                                            continue;
                                        }

                                        if let Some(text) =
                                            message.get("text").and_then(|t| t.as_str())
                                        {
                                            // Group chat filtering: only process if @bot is mentioned
                                            let chat_type = message
                                                .pointer("/chat/type")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("private");

                                            if !telegram_should_process_text_message(
                                                chat_type,
                                                text,
                                                &bot_username,
                                                topic_mode,
                                            ) {
                                                tracing::debug!("[Telegram] skipped group msg without @bot: {text}");
                                                continue;
                                            }

                                            // Strip @bot_username from command text (case-insensitive)
                                            let clean_text = strip_bot_mention(text, &bot_username);

                                            let sender_id = message
                                                .pointer("/from/id")
                                                .and_then(json_scalar_to_string)
                                                .unwrap_or_default();
                                            let target = telegram_message_target(
                                                channel_id,
                                                &configured_chat_id,
                                                topic_mode,
                                                message,
                                            );
                                            tracing::debug!("[Telegram] dispatching: {clean_text}");
                                            let send_result = command_tx
                                                .send(IncomingCommand {
                                                    channel_id,
                                                    sender_id,
                                                    command_text: clean_text,
                                                    callback_data: None,
                                                    target,
                                                    metadata: update.clone(),
                                                    voice_reply_lang: None,
                                                })
                                                .await;
                                            if let Err(e) = send_result {
                                                tracing::error!(
                                                    "[Telegram] command_tx.send failed: {e}"
                                                );
                                            }
                                        } else if let Some(voice_ref) =
                                            extract_voice_file(message)
                                        {
                                            let chat_type = message
                                                .pointer("/chat/type")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("private");
                                            if !telegram_should_process_voice_message(
                                                chat_type, topic_mode,
                                            ) {
                                                tracing::debug!(
                                                    "[Telegram] skipped voice msg in unbound group"
                                                );
                                                continue;
                                            }

                                            let sender_id = message
                                                .pointer("/from/id")
                                                .and_then(json_scalar_to_string)
                                                .unwrap_or_default();
                                            let target = telegram_message_target(
                                                channel_id,
                                                &configured_chat_id,
                                                topic_mode,
                                                message,
                                            );

                                            tracing::debug!(
                                                "[Telegram] downloading voice file {}",
                                                voice_ref.file_id
                                            );
                                            let audio_bytes = download_telegram_file(
                                                &client,
                                                &bot_token,
                                                &voice_ref.file_id,
                                            )
                                            .await;
                                            let audio_bytes = match audio_bytes {
                                                Ok(bytes) => bytes,
                                                Err(e) => {
                                                    tracing::warn!(
                                                        "[Telegram] voice file download failed: {e}"
                                                    );
                                                    send_plain_text_direct(
                                                        &client,
                                                        &bot_token,
                                                        &configured_chat_id,
                                                        &target,
                                                        i18n::voice_download_failed(Lang::Es),
                                                    )
                                                    .await;
                                                    continue;
                                                }
                                            };

                                            let transcription =
                                                speech_client.transcribe(audio_bytes).await;
                                            let stt = match transcription {
                                                Ok(stt) => stt,
                                                Err(e) => {
                                                    tracing::warn!(
                                                        "[Telegram] speech-to-text failed: {e}"
                                                    );
                                                    send_plain_text_direct(
                                                        &client,
                                                        &bot_token,
                                                        &configured_chat_id,
                                                        &target,
                                                        i18n::voice_service_unavailable(Lang::Es),
                                                    )
                                                    .await;
                                                    continue;
                                                }
                                            };
                                            if stt.text.trim().is_empty() {
                                                tracing::debug!(
                                                    "[Telegram] voice transcription was empty"
                                                );
                                                continue;
                                            }

                                            let reply_lang = crate::chat_channel::speech_service::pick_reply_language(
                                                stt.language.as_deref(),
                                            );
                                            tracing::debug!(
                                                "[Telegram] dispatching voice transcript: {}",
                                                stt.text
                                            );
                                            let send_result = command_tx
                                                .send(IncomingCommand {
                                                    channel_id,
                                                    sender_id,
                                                    command_text: stt.text,
                                                    callback_data: None,
                                                    target,
                                                    metadata: update.clone(),
                                                    voice_reply_lang: Some(reply_lang.to_string()),
                                                })
                                                .await;
                                            if let Err(e) = send_result {
                                                tracing::error!(
                                                    "[Telegram] command_tx.send failed: {e}"
                                                );
                                            }
                                        } else {
                                            tracing::info!(
                                                "[Telegram] message update without text or voice"
                                            );
                                        }
                                    } else if let Some(callback) = update.get("callback_query") {
                                        let Some(message) = callback.get("message") else {
                                            tracing::debug!(
                                                "[Telegram] skipped callback without message"
                                            );
                                            continue;
                                        };
                                        if !telegram_message_chat_matches(
                                            message,
                                            &configured_chat_id,
                                        ) {
                                            tracing::debug!(
                                                "[Telegram] skipped callback from unconfigured chat"
                                            );
                                            continue;
                                        }
                                        if let Some(callback_id) =
                                            callback.get("id").and_then(|v| v.as_str())
                                        {
                                            answer_callback_query(&client, &bot_token, callback_id)
                                                .await;
                                        }
                                        let Some(data) =
                                            callback.get("data").and_then(|v| v.as_str())
                                        else {
                                            tracing::debug!(
                                                "[Telegram] skipped callback without data"
                                            );
                                            continue;
                                        };
                                        let sender_id = callback
                                            .pointer("/from/id")
                                            .and_then(json_scalar_to_string)
                                            .unwrap_or_default();
                                        let target = telegram_message_target(
                                            channel_id,
                                            &configured_chat_id,
                                            topic_mode,
                                            message,
                                        );
                                        tracing::debug!("[Telegram] dispatching callback: {data}");
                                        let send_result = command_tx
                                            .send(IncomingCommand {
                                                channel_id,
                                                sender_id,
                                                command_text: data.to_string(),
                                                callback_data: Some(data.to_string()),
                                                target,
                                                metadata: update.clone(),
                                                voice_reply_lang: None,
                                            })
                                            .await;
                                        if let Err(e) = send_result {
                                            tracing::error!(
                                                "[Telegram] command_tx.send failed: {e}"
                                            );
                                        }
                                    } else {
                                        tracing::info!(
                                            "[Telegram] update without message/callback_query"
                                        );
                                    }
                                }
                            }
                        } else {
                            tracing::error!("[Telegram] failed to parse response body");
                        }
                    }
                    Err(e) => {
                        let msg = redact_token(e.to_string(), &bot_token);
                        tracing::error!("[Telegram] polling error: {msg}");
                        *status.lock().await = ChannelConnectionStatus::Error;
                        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    }
                }
            }
            *status.lock().await = ChannelConnectionStatus::Disconnected;
        });

        Ok(())
    }

    async fn stop(&self) -> Result<(), ChatChannelError> {
        if let Some(tx) = self.shutdown_tx.lock().await.take() {
            let _ = tx.send(true);
        }
        *self.status.lock().await = ChannelConnectionStatus::Disconnected;
        Ok(())
    }

    async fn status(&self) -> ChannelConnectionStatus {
        *self.status.lock().await
    }

    async fn send_message(&self, text: &str) -> Result<SentMessageId, ChatChannelError> {
        self.send_text(text, None, None).await
    }

    async fn send_rich_message(
        &self,
        message: &RichMessage,
    ) -> Result<SentMessageId, ChatChannelError> {
        self.send_rich_message_with_target(message, None).await
    }

    async fn send_rich_message_to(
        &self,
        message: &RichMessage,
        target: &ChannelMessageTarget,
    ) -> Result<SentMessageId, ChatChannelError> {
        self.send_rich_message_with_target(message, Some(target))
            .await
    }

    async fn send_interactive_message(
        &self,
        message: &InteractiveMessage,
    ) -> Result<SentMessageId, ChatChannelError> {
        self.send_interactive_message_with_target(message, None)
            .await
    }

    async fn send_interactive_message_to(
        &self,
        message: &InteractiveMessage,
        target: &ChannelMessageTarget,
    ) -> Result<SentMessageId, ChatChannelError> {
        self.send_interactive_message_with_target(message, Some(target))
            .await
    }

    async fn create_thread(&self, title: &str) -> Result<ChannelMessageTarget, ChatChannelError> {
        if !self.topic_mode {
            return Err(ChatChannelError::Unsupported(
                "Telegram topic mode is not enabled".to_string(),
            ));
        }

        // Use the canonical numeric id so the binding written from the returned
        // target matches inbound updates (which are keyed by numeric /chat/id).
        let chat_id = self.canonical_chat_id().await?;
        let body = serde_json::json!({
            "chat_id": chat_id.clone(),
            "name": telegram_topic_title(title),
        });
        let resp = self
            .client
            .post(self.api_url("createForumTopic"))
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                ChatChannelError::SendFailed(redact_token(e.to_string(), &self.bot_token))
            })?;
        let result: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| {
                ChatChannelError::SendFailed(redact_token(e.to_string(), &self.bot_token))
            })?;
        if result.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let desc = result
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("failed to create Telegram topic");
            return Err(ChatChannelError::SendFailed(desc.to_string()));
        }
        let thread_id = result
            .pointer("/result/message_thread_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| {
                ChatChannelError::SendFailed(
                    "Telegram createForumTopic returned no message_thread_id".to_string(),
                )
            })?;
        Ok(ChannelMessageTarget::telegram_forum_topic(
            self.channel_id,
            chat_id,
            thread_id.to_string(),
        ))
    }

    async fn edit_thread_title(
        &self,
        target: &ChannelMessageTarget,
        title: &str,
    ) -> Result<(), ChatChannelError> {
        if !target.is_telegram_forum_topic() {
            return Err(ChatChannelError::Unsupported(
                "target is not a Telegram forum topic".to_string(),
            ));
        }
        let thread_id = target
            .thread_key
            .as_deref()
            .and_then(|s| s.parse::<i64>().ok())
            .ok_or_else(|| ChatChannelError::SendFailed("invalid Telegram topic id".to_string()))?;
        let chat_id = target.chat_id.as_deref().unwrap_or(&self.chat_id);
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_thread_id": thread_id,
            "name": telegram_topic_title(title),
        });
        let resp = self
            .client
            .post(self.api_url("editForumTopic"))
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                ChatChannelError::SendFailed(redact_token(e.to_string(), &self.bot_token))
            })?;
        let result: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| {
                ChatChannelError::SendFailed(redact_token(e.to_string(), &self.bot_token))
            })?;
        if result.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            Ok(())
        } else {
            let desc = result
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("failed to edit Telegram topic");
            Err(ChatChannelError::SendFailed(desc.to_string()))
        }
    }

    async fn test_connection(&self) -> Result<(), ChatChannelError> {
        let resp = self
            .client
            .get(self.api_url("getMe"))
            .send()
            .await
            .map_err(|e| {
                ChatChannelError::ConnectionFailed(redact_token(e.to_string(), &self.bot_token))
            })?;

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| {
                ChatChannelError::ConnectionFailed(redact_token(e.to_string(), &self.bot_token))
            })?;

        if body.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            Ok(())
        } else {
            let desc = body
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("Invalid bot token");
            Err(ChatChannelError::AuthenticationFailed(desc.to_string()))
        }
    }

    async fn send_document(
        &self,
        target: &ChannelMessageTarget,
        bytes: Vec<u8>,
        filename: &str,
        caption: Option<&str>,
    ) -> Result<SentMessageId, ChatChannelError> {
        let mime = guess_mime(filename);
        self.upload_media(
            "sendDocument",
            "document",
            target,
            bytes,
            filename,
            mime,
            caption,
        )
        .await
    }

    async fn send_photo(
        &self,
        target: &ChannelMessageTarget,
        bytes: Vec<u8>,
        filename: &str,
        caption: Option<&str>,
    ) -> Result<SentMessageId, ChatChannelError> {
        let mime = guess_mime(filename);
        self.upload_media("sendPhoto", "photo", target, bytes, filename, mime, caption)
            .await
    }

    async fn send_audio(
        &self,
        target: &ChannelMessageTarget,
        bytes: Vec<u8>,
        filename: &str,
        caption: Option<&str>,
    ) -> Result<SentMessageId, ChatChannelError> {
        let mime = guess_mime(filename);
        self.upload_media("sendAudio", "audio", target, bytes, filename, mime, caption)
            .await
    }

    async fn send_voice(
        &self,
        target: &ChannelMessageTarget,
        bytes: Vec<u8>,
    ) -> Result<SentMessageId, ChatChannelError> {
        self.upload_media(
            "sendVoice",
            "voice",
            target,
            bytes,
            "voice.ogg",
            Some("audio/ogg"),
            None,
        )
        .await
    }
}

/// Best-effort mime type from a file's extension, for the multipart upload's
/// `Content-Type`. `None` lets Telegram infer it from the file name instead
/// of failing the upload over an unrecognized extension.
fn guess_mime(filename: &str) -> Option<&'static str> {
    let ext = filename.rsplit('.').next()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "opus" => "audio/ogg",
        "m4a" => "audio/mp4",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "json" => "application/json",
        "zip" => "application/zip",
        _ => return None,
    })
}

/// Scrub the bot token from an error/log string before it escapes.
///
/// Telegram API URLs embed the token in the path (`/bot<TOKEN>/…`) and
/// `reqwest::Error`'s `Display` appends `for url (<url>)`. That text is written
/// to logs and — for topic create/edit failures — sent into the chat itself, so
/// the raw token must never survive stringification. No-op on an empty token.
fn redact_token(msg: String, token: &str) -> String {
    if token.is_empty() {
        msg
    } else {
        msg.replace(token, "***")
    }
}

/// Byte offset of the first case-insensitive `at_bot` in `text`, or `None`.
///
/// Searches `text` itself, so the offset it returns is a real byte offset into
/// `text`. Searching a `to_lowercase()` copy instead is what this used to do,
/// and it is unsound: lowercasing is not length preserving, so one character
/// before the mention silently shifts every later offset. `İ` (U+0130, two
/// bytes) lowercases to two code points (three bytes), the Kelvin sign `K`
/// (three bytes) lowercases to `k` (one byte), and `ẞ` (three bytes) lowercases
/// to `ß` (two). Slicing `text` at an offset found that way then lands in the
/// middle of a character, or past the end, and panics.
///
/// `eq_ignore_ascii_case` is the whole comparison because a Telegram bot
/// username is ASCII by definition (letters, digits and underscores, ending in
/// `bot`), so there is no non-ASCII folding to do on the needle.
///
/// `str::get` rather than indexing: it answers `None` for an end offset that is
/// out of range or not on a character boundary, which is exactly the case that
/// used to panic.
fn find_bot_mention(text: &str, at_bot: &str) -> Option<usize> {
    text.char_indices().map(|(i, _)| i).find(|&i| {
        text.get(i..i + at_bot.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(at_bot))
    })
}

/// Strip `@bot_username` from text (case-insensitive).
/// Handles Telegram convention: `/command@botname args` → `/command args`
fn strip_bot_mention(text: &str, bot_username: &str) -> String {
    if bot_username.is_empty() {
        return text.to_string();
    }
    let at_bot = format!("@{}", bot_username);
    match find_bot_mention(text, &at_bot) {
        // `find_bot_mention` only reports an offset whose end is a character
        // boundary, so both slices below are in range and aligned.
        Some(pos) => {
            let mut result = String::with_capacity(text.len());
            result.push_str(&text[..pos]);
            result.push_str(&text[pos + at_bot.len()..]);
            result.trim().to_string()
        }
        None => text.to_string(),
    }
}

async fn answer_callback_query(client: &reqwest::Client, bot_token: &str, callback_query_id: &str) {
    let body = serde_json::json!({
        "callback_query_id": callback_query_id,
    });
    let result = client
        .post(format!(
            "https://api.telegram.org/bot{}/answerCallbackQuery",
            bot_token
        ))
        .json(&body)
        .send()
        .await;
    if let Err(e) = result {
        let msg = redact_token(e.to_string(), bot_token);
        tracing::warn!("[Telegram] answerCallbackQuery failed: {msg}");
    }
}

/// Bot API's own cap on files a bot may download via `getFile`.
const TELEGRAM_MAX_DOWNLOAD_BYTES: usize = 20 * 1024 * 1024;

/// Download a voice/audio/video-note file's raw bytes: `getFile` to resolve
/// `file_id` into a `file_path`, then GET the file itself. Defensive about
/// the 20MB Bot API cap even though Telegram itself already enforces it on
/// `getFile`.
async fn download_telegram_file(
    client: &reqwest::Client,
    bot_token: &str,
    file_id: &str,
) -> Result<Vec<u8>, ChatChannelError> {
    let get_file_body = serde_json::json!({ "file_id": file_id });
    let resp = client
        .post(format!("https://api.telegram.org/bot{bot_token}/getFile"))
        .json(&get_file_body)
        .send()
        .await
        .map_err(|e| ChatChannelError::SendFailed(redact_token(e.to_string(), bot_token)))?;
    let result: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| ChatChannelError::SendFailed(redact_token(e.to_string(), bot_token)))?;
    if result.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let desc = result
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("getFile failed");
        return Err(ChatChannelError::SendFailed(desc.to_string()));
    }
    let file_path = result
        .pointer("/result/file_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ChatChannelError::SendFailed("Telegram getFile returned no file_path".to_string())
        })?;

    let download_url = format!("https://api.telegram.org/file/bot{bot_token}/{file_path}");
    let resp = client
        .get(&download_url)
        .send()
        .await
        .map_err(|e| ChatChannelError::SendFailed(redact_token(e.to_string(), bot_token)))?;
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| ChatChannelError::SendFailed(redact_token(e.to_string(), bot_token)))?;
    if bytes.len() > TELEGRAM_MAX_DOWNLOAD_BYTES {
        return Err(ChatChannelError::SendFailed(format!(
            "voice file too large ({} bytes, limit {})",
            bytes.len(),
            TELEGRAM_MAX_DOWNLOAD_BYTES
        )));
    }
    Ok(bytes.to_vec())
}

/// Send a plain text message directly, bypassing the command dispatcher —
/// used for out-of-band notices (e.g. "voice service unavailable") about an
/// inbound update that never became an `IncomingCommand`.
async fn send_plain_text_direct(
    client: &reqwest::Client,
    bot_token: &str,
    default_chat_id: &str,
    target: &ChannelMessageTarget,
    text: &str,
) {
    let Ok(body) = telegram_send_message_body(default_chat_id, text, None, Some(target), None)
    else {
        return;
    };
    let result = client
        .post(format!("https://api.telegram.org/bot{bot_token}/sendMessage"))
        .json(&body)
        .send()
        .await;
    if let Err(e) = result {
        let msg = redact_token(e.to_string(), bot_token);
        tracing::warn!("[Telegram] direct sendMessage failed: {msg}");
    }
}

fn telegram_message_chat_matches(message: &serde_json::Value, configured_chat_id: &str) -> bool {
    let configured = configured_chat_id.trim();
    if configured.is_empty() {
        return false;
    }

    if message
        .pointer("/chat/id")
        .and_then(json_scalar_to_string)
        .as_deref()
        == Some(configured)
    {
        return true;
    }

    let configured_username = configured.strip_prefix('@').unwrap_or(configured);
    message
        .pointer("/chat/username")
        .and_then(|v| v.as_str())
        .is_some_and(|username| username.eq_ignore_ascii_case(configured_username))
}

fn telegram_message_target(
    channel_id: i32,
    configured_chat_id: &str,
    topic_mode: bool,
    message: &serde_json::Value,
) -> ChannelMessageTarget {
    if !topic_mode {
        return ChannelMessageTarget::channel(channel_id);
    }

    let chat_id = message
        .pointer("/chat/id")
        .and_then(json_scalar_to_string)
        .unwrap_or_else(|| configured_chat_id.to_string());

    if let Some(thread_key) = message
        .pointer("/message_thread_id")
        .and_then(json_scalar_to_string)
    {
        ChannelMessageTarget::telegram_forum_topic(channel_id, chat_id, thread_key)
    } else {
        ChannelMessageTarget::telegram_general(channel_id, chat_id)
    }
}

fn telegram_should_process_text_message(
    chat_type: &str,
    text: &str,
    bot_username: &str,
    topic_mode: bool,
) -> bool {
    if topic_mode || bot_username.is_empty() || (chat_type != "group" && chat_type != "supergroup")
    {
        return true;
    }

    let at_bot = format!("@{}", bot_username);
    // Same matcher `strip_bot_mention` uses, so a mention this accepts is one
    // that will actually be stripped, and neither allocates a lowercased copy
    // of every group message on the long-poll path.
    find_bot_mention(text, &at_bot).is_some()
}

/// A voice note (`message.voice`), audio file (`message.audio`), or video
/// note (`message.video_note`) can never carry an `@bot` mention the way a
/// text message can, so in an unbound group (no topic mode) there is no way
/// for the sender to address the bot explicitly — those are skipped rather
/// than guessed at. Everything else (private chats, topic-mode groups)
/// processes normally, same as `telegram_should_process_text_message`.
fn telegram_should_process_voice_message(chat_type: &str, topic_mode: bool) -> bool {
    topic_mode || (chat_type != "group" && chat_type != "supergroup")
}

/// A Telegram `file_id` pulled from `message.voice`, `message.audio`, or
/// `message.video_note`, whichever is present (checked in that order).
#[derive(Debug, Clone, PartialEq, Eq)]
struct TelegramVoiceRef {
    file_id: String,
}

fn extract_voice_file(message: &serde_json::Value) -> Option<TelegramVoiceRef> {
    for field in ["voice", "audio", "video_note"] {
        if let Some(file_id) = message.pointer(&format!("/{field}/file_id")).and_then(|v| v.as_str())
        {
            return Some(TelegramVoiceRef {
                file_id: file_id.to_string(),
            });
        }
    }
    None
}

fn json_scalar_to_string(value: &serde_json::Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        Some(s.to_string())
    } else if let Some(i) = value.as_i64() {
        Some(i.to_string())
    } else {
        value.as_u64().map(|u| u.to_string())
    }
}

fn telegram_topic_title(title: &str) -> String {
    let title = title.trim();
    let title = if title.is_empty() {
        "Phantom session"
    } else {
        title
    };
    title.chars().take(128).collect()
}

fn telegram_inline_keyboard(message: &InteractiveMessage) -> Option<serde_json::Value> {
    if message.buttons.is_empty() {
        return None;
    }

    let rows: Vec<serde_json::Value> = message
        .buttons
        .chunks(2)
        .map(|chunk| {
            serde_json::Value::Array(
                chunk
                    .iter()
                    .map(|button| {
                        serde_json::json!({
                            "text": button.label,
                            "callback_data": button.id,
                        })
                    })
                    .collect(),
            )
        })
        .collect();

    Some(serde_json::json!({ "inline_keyboard": rows }))
}

fn telegram_send_message_body(
    default_chat_id: &str,
    text: &str,
    parse_mode: Option<&str>,
    target: Option<&ChannelMessageTarget>,
    reply_markup: Option<serde_json::Value>,
) -> Result<serde_json::Value, ChatChannelError> {
    let chat_id = target
        .and_then(|t| t.chat_id.as_deref())
        .unwrap_or(default_chat_id);
    let mut body = serde_json::json!({
        "chat_id": chat_id,
        "text": text,
    });
    if let Some(mode) = parse_mode {
        body["parse_mode"] = serde_json::Value::String(mode.to_string());
    }
    if let Some(markup) = reply_markup {
        body["reply_markup"] = markup;
    }
    if let Some(target) = target {
        if target.is_telegram_forum_topic() {
            let thread_id = target
                .thread_key
                .as_deref()
                .and_then(|s| s.parse::<i64>().ok())
                .ok_or_else(|| {
                    ChatChannelError::SendFailed(
                        "invalid Telegram message_thread_id target".to_string(),
                    )
                })?;
            body["message_thread_id"] = serde_json::json!(thread_id);
        }
    }

    Ok(body)
}

fn format_telegram_markdown(msg: &RichMessage) -> String {
    let mut text = String::new();

    let level_emoji = match msg.level {
        MessageLevel::Info => "ℹ️",
        MessageLevel::Warning => "⚠️",
        MessageLevel::Error => "❌",
    };

    if let Some(title) = &msg.title {
        text.push_str(&format!("{} *{}*\n", level_emoji, escape_markdown(title)));
    }

    text.push_str(&escape_markdown(&msg.body));

    if !msg.fields.is_empty() {
        text.push('\n');
        for (key, value) in &msg.fields {
            text.push_str(&format!(
                "\n*{}*: {}",
                escape_markdown(key),
                escape_markdown(value)
            ));
        }
    }

    text
}

fn escape_markdown(text: &str) -> String {
    // Backslash must be escaped first to avoid double-escaping
    text.replace('\\', "\\\\")
        .replace('_', "\\_")
        .replace('*', "\\*")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('(', "\\(")
        .replace(')', "\\)")
        .replace('~', "\\~")
        .replace('`', "\\`")
        .replace('>', "\\>")
        .replace('#', "\\#")
        .replace('+', "\\+")
        .replace('-', "\\-")
        .replace('=', "\\=")
        .replace('|', "\\|")
        .replace('{', "\\{")
        .replace('}', "\\}")
        .replace('.', "\\.")
        .replace('!', "\\!")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_token_scrubs_token_from_error_url() {
        let token = "123456:AAExampleSecretToken";
        // Mirrors reqwest's `Display`, which appends the request URL (with the
        // token in the path) to transport errors.
        let leaked = format!(
            "error sending request for url (https://api.telegram.org/bot{token}/createForumTopic)"
        );
        let scrubbed = redact_token(leaked, token);
        assert!(!scrubbed.contains(token), "token must be scrubbed: {scrubbed}");
        assert!(scrubbed.contains("bot***/createForumTopic"), "{scrubbed}");
    }

    #[test]
    fn redact_token_is_noop_for_empty_token() {
        let msg = "some error without any token".to_string();
        assert_eq!(redact_token(msg.clone(), ""), msg);
    }

    #[tokio::test]
    async fn canonical_chat_id_passes_numeric_through_without_network() {
        // A numeric chat_id short-circuits before any getChat call, so this
        // resolves offline. (The `@username` → getChat branch is covered by
        // real-device / HTTP-mock testing.)
        let backend = TelegramBackend::new(1, "token".into(), "-100123".into(), true);
        assert_eq!(backend.canonical_chat_id().await.unwrap(), "-100123");
    }

    #[test]
    fn chat_filter_matches_configured_numeric_chat() {
        let message = serde_json::json!({
            "chat": { "id": -100123, "type": "supergroup" }
        });

        assert!(telegram_message_chat_matches(&message, "-100123"));
        assert!(!telegram_message_chat_matches(&message, "-100456"));
    }

    #[test]
    fn chat_filter_matches_configured_username_case_insensitively() {
        let message = serde_json::json!({
            "chat": { "id": -100123, "username": "CodegTopics" }
        });

        assert!(telegram_message_chat_matches(&message, "@codegtopics"));
        assert!(telegram_message_chat_matches(&message, "CODEGTOPICS"));
        assert!(!telegram_message_chat_matches(&message, "other"));
    }

    #[test]
    fn target_parser_uses_channel_target_when_topic_mode_is_disabled() {
        let message = serde_json::json!({
            "chat": { "id": -100123 },
            "message_thread_id": 8
        });

        let target = telegram_message_target(7, "-100123", false, &message);

        assert_eq!(target, ChannelMessageTarget::channel(7));
    }

    #[test]
    fn target_parser_distinguishes_general_and_forum_topics() {
        let general = serde_json::json!({ "chat": { "id": -100123 } });
        let topic = serde_json::json!({
            "chat": { "id": -100123 },
            "message_thread_id": 2
        });

        assert_eq!(
            telegram_message_target(7, "-100123", true, &general),
            ChannelMessageTarget::telegram_general(7, "-100123")
        );
        assert_eq!(
            telegram_message_target(7, "-100123", true, &topic),
            ChannelMessageTarget::telegram_forum_topic(7, "-100123", "2")
        );
    }

    #[test]
    fn text_filter_preserves_legacy_group_mention_requirement() {
        assert!(telegram_should_process_text_message(
            "supergroup",
            "/task@codeg_bot build",
            "codeg_bot",
            false
        ));
        assert!(!telegram_should_process_text_message(
            "supergroup",
            "/task build",
            "codeg_bot",
            false
        ));
    }

    #[test]
    fn strip_bot_mention_removes_the_mention_in_either_case() {
        assert_eq!(
            strip_bot_mention("/task@codeg_bot build", "codeg_bot"),
            "/task build"
        );
        assert_eq!(
            strip_bot_mention("/task@CodeG_Bot build", "codeg_bot"),
            "/task build"
        );
        assert_eq!(strip_bot_mention("/task build", "codeg_bot"), "/task build");
        assert_eq!(strip_bot_mention("/task@codeg_bot", ""), "/task@codeg_bot");
    }

    /// The long-poll loop runs unattended, and any member of a bound group
    /// chooses the text it parses. Finding the mention in a `to_lowercase()`
    /// copy and then slicing the original panicked on every case below, because
    /// lowercasing is not length preserving: `İ` grows by a byte, `K` shrinks
    /// by two, `ẞ` shrinks by one. The offsets then land mid character or past
    /// the end of the string.
    #[test]
    fn strip_bot_mention_survives_text_whose_lowercase_is_a_different_length() {
        // capital I with dot above, 2 bytes -> 3
        assert_eq!(
            strip_bot_mention("\u{130}@codeg_bot hi", "codeg_bot"),
            "\u{130} hi"
        );
        // Kelvin sign, 3 bytes -> 1
        assert_eq!(
            strip_bot_mention("\u{212A}@codeg_bot hi", "codeg_bot"),
            "\u{212A} hi"
        );
        // capital sharp s, 3 bytes -> 2
        assert_eq!(
            strip_bot_mention("\u{1E9E}@codeg_bot hi", "codeg_bot"),
            "\u{1E9E} hi"
        );
        // Enough drift to run the old offset off the end of the string.
        assert_eq!(
            strip_bot_mention("\u{130}\u{130}@codeg_bot", "codeg_bot"),
            "\u{130}\u{130}"
        );
        assert_eq!(
            strip_bot_mention("\u{212A}\u{212A}@codeg_bot x", "codeg_bot"),
            "\u{212A}\u{212A} x"
        );
        // A mention whose tail is a wide character is not a mention, and
        // deciding that must not slice through the character.
        assert_eq!(
            strip_bot_mention("@codeg_bo\u{65E5}", "codeg_bot"),
            "@codeg_bo\u{65E5}"
        );
    }

    #[test]
    fn text_filter_allows_unmentioned_text_in_topic_mode() {
        assert!(telegram_should_process_text_message(
            "supergroup",
            "plain follow-up",
            "codeg_bot",
            true
        ));
    }

    #[test]
    fn send_message_body_includes_forum_topic_thread_id() {
        let target = ChannelMessageTarget::telegram_forum_topic(7, "-100123", "42");
        let body = telegram_send_message_body(
            "fallback",
            "hello",
            Some("MarkdownV2"),
            Some(&target),
            Some(serde_json::json!({ "inline_keyboard": [] })),
        )
        .expect("body");

        assert_eq!(body["chat_id"], "-100123");
        assert_eq!(body["message_thread_id"], 42);
        assert_eq!(body["parse_mode"], "MarkdownV2");
        assert_eq!(
            body["reply_markup"],
            serde_json::json!({ "inline_keyboard": [] })
        );
    }

    #[test]
    fn send_message_body_rejects_invalid_forum_topic_thread_id() {
        let target = ChannelMessageTarget::telegram_forum_topic(7, "-100123", "bad");
        let err = telegram_send_message_body("fallback", "hello", None, Some(&target), None)
            .expect_err("invalid thread id should fail");

        assert!(err
            .to_string()
            .contains("invalid Telegram message_thread_id target"));
    }

    #[test]
    fn inline_keyboard_uses_callback_data_in_two_button_rows() {
        let message = InteractiveMessage {
            base: RichMessage::info("Pick"),
            buttons: vec![
                MessageButton {
                    id: "cfg:folder:1".to_string(),
                    label: "One".to_string(),
                    style: ButtonStyle::Default,
                },
                MessageButton {
                    id: "cfg:folder:2".to_string(),
                    label: "Two".to_string(),
                    style: ButtonStyle::Default,
                },
                MessageButton {
                    id: "cfg:folder:3".to_string(),
                    label: "Three".to_string(),
                    style: ButtonStyle::Default,
                },
            ],
            callback_context: serde_json::json!({}),
        };

        let keyboard = telegram_inline_keyboard(&message).expect("keyboard");

        assert_eq!(keyboard["inline_keyboard"].as_array().unwrap().len(), 2);
        assert_eq!(
            keyboard["inline_keyboard"][0][0]["callback_data"],
            "cfg:folder:1"
        );
        assert_eq!(
            keyboard["inline_keyboard"][0][1]["callback_data"],
            "cfg:folder:2"
        );
        assert_eq!(
            keyboard["inline_keyboard"][1][0]["callback_data"],
            "cfg:folder:3"
        );
    }

    #[test]
    fn guess_mime_recognizes_common_extensions_case_insensitively() {
        assert_eq!(guess_mime("photo.PNG"), Some("image/png"));
        assert_eq!(guess_mime("clip.jpg"), Some("image/jpeg"));
        assert_eq!(guess_mime("clip.jpeg"), Some("image/jpeg"));
        assert_eq!(guess_mime("note.ogg"), Some("audio/ogg"));
        assert_eq!(guess_mime("report.pdf"), Some("application/pdf"));
    }

    #[test]
    fn guess_mime_is_none_for_unknown_or_missing_extension() {
        assert_eq!(guess_mime("no_extension"), None);
        assert_eq!(guess_mime("archive.tar.gz"), None);
    }

    #[test]
    fn extract_voice_file_reads_voice_field() {
        let message = serde_json::json!({ "voice": { "file_id": "abc123", "duration": 5 } });
        assert_eq!(
            extract_voice_file(&message),
            Some(TelegramVoiceRef {
                file_id: "abc123".to_string()
            })
        );
    }

    #[test]
    fn extract_voice_file_reads_audio_field_when_no_voice() {
        let message = serde_json::json!({ "audio": { "file_id": "aud1" } });
        assert_eq!(
            extract_voice_file(&message),
            Some(TelegramVoiceRef {
                file_id: "aud1".to_string()
            })
        );
    }

    #[test]
    fn extract_voice_file_reads_video_note_field_as_last_resort() {
        let message = serde_json::json!({ "video_note": { "file_id": "vn1" } });
        assert_eq!(
            extract_voice_file(&message),
            Some(TelegramVoiceRef {
                file_id: "vn1".to_string()
            })
        );
    }

    #[test]
    fn extract_voice_file_prefers_voice_over_audio_and_video_note() {
        let message = serde_json::json!({
            "voice": { "file_id": "v" },
            "audio": { "file_id": "a" },
            "video_note": { "file_id": "vn" },
        });
        assert_eq!(
            extract_voice_file(&message).map(|r| r.file_id),
            Some("v".to_string())
        );
    }

    #[test]
    fn extract_voice_file_is_none_for_a_text_message() {
        let message = serde_json::json!({ "text": "hello" });
        assert_eq!(extract_voice_file(&message), None);
    }

    #[test]
    fn should_process_voice_in_private_chats_and_topic_mode_groups() {
        assert!(telegram_should_process_voice_message("private", false));
        assert!(telegram_should_process_voice_message("supergroup", true));
        assert!(telegram_should_process_voice_message("group", true));
    }

    #[test]
    fn should_skip_voice_in_a_plain_group_without_topic_mode() {
        assert!(!telegram_should_process_voice_message("group", false));
        assert!(!telegram_should_process_voice_message("supergroup", false));
    }
}
