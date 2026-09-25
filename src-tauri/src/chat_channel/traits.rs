use async_trait::async_trait;
use tokio::sync::mpsc;

use super::error::ChatChannelError;
use super::types::*;

#[async_trait]
pub trait ChatChannelBackend: Send + Sync + 'static {
    fn channel_type(&self) -> ChannelType;

    /// Start the receiving loop. `command_tx` forwards incoming IM messages
    /// to the central command dispatcher.
    async fn start(
        &self,
        command_tx: mpsc::Sender<IncomingCommand>,
    ) -> Result<(), ChatChannelError>;

    /// Stop the backend connection gracefully.
    async fn stop(&self) -> Result<(), ChatChannelError>;

    /// Current connection status.
    async fn status(&self) -> ChannelConnectionStatus;

    /// Send a plain text message.
    async fn send_message(&self, text: &str) -> Result<SentMessageId, ChatChannelError>;

    /// Send a rich/structured message (Telegram Markdown / Lark Card).
    async fn send_rich_message(
        &self,
        message: &RichMessage,
    ) -> Result<SentMessageId, ChatChannelError>;

    /// Send a rich message to a provider-specific thread/topic target.
    /// Backends without thread semantics keep the existing channel-level behavior.
    async fn send_rich_message_to(
        &self,
        message: &RichMessage,
        _target: &ChannelMessageTarget,
    ) -> Result<SentMessageId, ChatChannelError> {
        self.send_rich_message(message).await
    }

    /// Create a provider-specific thread/topic target.
    async fn create_thread(
        &self,
        _title: &str,
    ) -> Result<ChannelMessageTarget, ChatChannelError> {
        Err(ChatChannelError::Unsupported(
            "thread creation is not supported by this channel".to_string(),
        ))
    }

    /// Best-effort provider-side title sync for a bound thread/topic.
    async fn edit_thread_title(
        &self,
        _target: &ChannelMessageTarget,
        _title: &str,
    ) -> Result<(), ChatChannelError> {
        Err(ChatChannelError::Unsupported(
            "thread title editing is not supported by this channel".to_string(),
        ))
    }

    /// [Phase 2] Send an interactive message with action buttons.
    /// Default implementation degrades to send_rich_message.
    async fn send_interactive_message(
        &self,
        message: &InteractiveMessage,
    ) -> Result<SentMessageId, ChatChannelError> {
        self.send_rich_message(&message.to_rich_fallback()).await
    }

    /// Send an interactive message to a provider-specific thread/topic target.
    async fn send_interactive_message_to(
        &self,
        message: &InteractiveMessage,
        _target: &ChannelMessageTarget,
    ) -> Result<SentMessageId, ChatChannelError> {
        self.send_interactive_message(message).await
    }

    /// [Phase 2] Update an already-sent message (e.g., permission status change).
    async fn update_message(
        &self,
        _message_id: &SentMessageId,
        _message: &RichMessage,
    ) -> Result<(), ChatChannelError> {
        Ok(())
    }

    /// Test the connection (used by "Test Connection" button in UI).
    async fn test_connection(&self) -> Result<(), ChatChannelError>;

    /// Upload a generic file as a document attachment to a thread/topic
    /// target. Default degrades to `Unsupported` for backends with no
    /// document-upload API (only Telegram implements this today).
    async fn send_document(
        &self,
        _target: &ChannelMessageTarget,
        _bytes: Vec<u8>,
        _filename: &str,
        _caption: Option<&str>,
    ) -> Result<SentMessageId, ChatChannelError> {
        Err(ChatChannelError::Unsupported(
            "document upload is not supported by this channel".to_string(),
        ))
    }

    /// Upload an image, rendered inline by the client rather than as a
    /// generic attachment where the backend distinguishes the two.
    async fn send_photo(
        &self,
        _target: &ChannelMessageTarget,
        _bytes: Vec<u8>,
        _filename: &str,
        _caption: Option<&str>,
    ) -> Result<SentMessageId, ChatChannelError> {
        Err(ChatChannelError::Unsupported(
            "photo upload is not supported by this channel".to_string(),
        ))
    }

    /// Upload a playable audio file (as opposed to a generic document).
    async fn send_audio(
        &self,
        _target: &ChannelMessageTarget,
        _bytes: Vec<u8>,
        _filename: &str,
        _caption: Option<&str>,
    ) -> Result<SentMessageId, ChatChannelError> {
        Err(ChatChannelError::Unsupported(
            "audio upload is not supported by this channel".to_string(),
        ))
    }

    /// Upload a synthesized spoken reply as a native "voice message" where
    /// the backend has that concept (Telegram's `sendVoice`, OGG/Opus only).
    async fn send_voice(
        &self,
        _target: &ChannelMessageTarget,
        _bytes: Vec<u8>,
    ) -> Result<SentMessageId, ChatChannelError> {
        Err(ChatChannelError::Unsupported(
            "voice upload is not supported by this channel".to_string(),
        ))
    }
}
