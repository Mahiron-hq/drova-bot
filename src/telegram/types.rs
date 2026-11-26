use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
#[allow(clippy::struct_field_names)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
    pub callback_query: Option<CallbackQuery>,
    pub my_chat_member: Option<ChatMemberUpdated>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct User {
    pub id: i64,
    #[serde(default)]
    pub is_bot: bool,
    pub username: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Document {
    pub file_id: String,
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
    pub file_size: Option<i64>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(clippy::struct_field_names)]
pub struct Message {
    pub message_id: i64,
    pub from: Option<User>,
    pub chat: Chat,
    pub text: Option<String>,
    pub document: Option<Document>,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: User,
    /// May be an inaccessible message; only id, chat and text are read.
    pub message: Option<Message>,
    pub data: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChatMember {
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct ChatMemberUpdated {
    pub chat: Chat,
    pub from: User,
    pub new_chat_member: ChatMember,
}

#[derive(Debug, Deserialize)]
pub struct SentMessage {
    pub message_id: i64,
}

#[derive(Debug, Deserialize)]
pub struct File {
    pub file_path: Option<String>,
    pub file_size: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Button {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub callback_data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub url: Option<String>,
}

impl Button {
    pub fn cb(text: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            callback_data: Some(data.into()),
            url: None,
        }
    }

    pub fn url(text: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            callback_data: None,
            url: Some(url.into()),
        }
    }
}

pub type Inline = Vec<Vec<Button>>;

#[derive(Debug, Clone)]
pub enum Markup {
    Inline(Inline),
    Reply(Vec<Vec<String>>, String),
}

impl Markup {
    pub fn to_json(&self) -> Value {
        match self {
            Self::Inline(rows) => json!({ "inline_keyboard": rows }),
            Self::Reply(rows, placeholder) => {
                let keyboard: Vec<Vec<Value>> = rows
                    .iter()
                    .map(|r| r.iter().map(|t| json!({ "text": t })).collect())
                    .collect();
                json!({
                    "keyboard": keyboard,
                    "resize_keyboard": true,
                    "is_persistent": true,
                    "input_field_placeholder": placeholder,
                })
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BotCommand {
    pub command: &'static str,
    pub description: &'static str,
}
