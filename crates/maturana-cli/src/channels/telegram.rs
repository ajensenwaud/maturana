use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(super) struct TelegramGetMeResponse {
    pub(super) ok: bool,
    pub(super) result: Option<TelegramUser>,
}

#[derive(Debug, Deserialize)]
pub(super) struct TelegramUser {
    pub(super) username: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct TelegramUpdatesResponse {
    pub(super) ok: bool,
    pub(super) result: Vec<TelegramUpdate>,
}

#[derive(Debug, Deserialize)]
pub(super) struct TelegramUpdate {
    pub(super) update_id: i64,
    pub(super) message: Option<TelegramMessage>,
    pub(super) channel_post: Option<TelegramMessage>,
    /// Set when the user taps an inline-keyboard button (e.g. the model selector).
    #[serde(default)]
    pub(super) callback_query: Option<TelegramCallbackQuery>,
}

/// A tap on an inline-keyboard button. `data` carries our `action:value` payload
/// (e.g. `model:gpt-5`); `message` is the bot message the keyboard is attached to,
/// which gives us the chat id (for the pairing gate) and message id (to edit).
#[derive(Debug, Deserialize)]
pub(super) struct TelegramCallbackQuery {
    pub(super) id: String,
    #[serde(default)]
    pub(super) data: Option<String>,
    #[serde(default)]
    pub(super) message: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
pub(super) struct TelegramMessage {
    pub(super) message_id: i64,
    pub(super) text: Option<String>,
    #[serde(default)]
    pub(super) caption: Option<String>,
    #[serde(default)]
    pub(super) document: Option<TelegramDocument>,
    #[serde(default)]
    pub(super) photo: Option<Vec<TelegramPhotoSize>>,
    #[serde(default)]
    pub(super) voice: Option<TelegramVoice>,
    #[serde(default)]
    pub(super) audio: Option<TelegramAudio>,
    pub(super) chat: TelegramChat,
}

/// A Telegram voice note (an OGG/Opus recording from the mic button). Carries no
/// `text`; we download and transcribe it (STT) host-side. Extra fields
/// (duration, mime_type, file_size) are ignored.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(super) struct TelegramVoice {
    pub(super) file_id: String,
}

/// A Telegram audio file (a music/audio attachment, as opposed to a voice note).
/// Also transcribed; the original file name, when present, is a better STT format
/// hint than the generic voice default.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(super) struct TelegramAudio {
    pub(super) file_id: String,
    #[serde(default)]
    pub(super) file_name: Option<String>,
}

/// A Telegram document attachment (file upload). The bot API caps `getFile`
/// downloads at 20 MB, so anything larger is refused up front.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(super) struct TelegramDocument {
    pub(super) file_id: String,
    #[serde(default)]
    pub(super) file_name: Option<String>,
    #[serde(default)]
    pub(super) file_size: Option<i64>,
}

/// One size of a Telegram photo upload. Telegram sends an ascending array of
/// sizes (thumbnail -> original); we OCR the largest. Extra fields (file_size,
/// width, height) are ignored.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(super) struct TelegramPhotoSize {
    pub(super) file_id: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct TelegramSendResponse {
    pub(super) ok: bool,
    pub(super) result: Option<TelegramSentMessage>,
}

#[derive(Debug, Deserialize)]
pub(super) struct TelegramOkResponse {
    pub(super) ok: bool,
}

#[derive(Debug, Deserialize)]
pub(super) struct TelegramSentMessage {
    pub(super) message_id: i64,
}

#[derive(Debug, Deserialize)]
pub(super) struct TelegramChat {
    pub(super) id: i64,
}
