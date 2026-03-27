use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantCredential {
    #[serde(default, alias = "botToken")]
    pub bot_token: Option<String>,
    #[serde(default, alias = "baseUrl")]
    pub api_base_url: Option<String>,
    #[serde(default, alias = "accountId")]
    pub account_id: Option<String>,
    #[serde(default, alias = "userId")]
    pub user_id: Option<String>,
    #[serde(default, alias = "syncBuf", alias = "sync_buf")]
    pub sync_buf: Option<String>,
    #[serde(rename = "gateway_url", default)]
    pub lowcode_ws_base_url: Option<String>,
    #[serde(rename = "gateway_token", default)]
    pub lowcode_ws_token: Option<String>,
    #[serde(
        rename = "outbound_token",
        default,
        alias = "outboundToken",
        alias = "outboundCallbackToken",
        alias = "callbackToken",
        alias = "channelCallbackToken"
    )]
    pub outbound_token: Option<String>,
    #[serde(default)]
    pub lowcode_forward_enabled: Option<bool>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

impl TenantCredential {
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    pub fn is_logged_in(&self) -> bool {
        self.bot_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_some()
    }

    pub fn ensure_outbound_token(&mut self) -> bool {
        if self
            .outbound_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_some()
        {
            return false;
        }

        self.outbound_token = Some(generate_outbound_token());
        true
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TenantSummary {
    pub tenant_id: String,
    pub logged_in: bool,
    pub bot_token_masked: Option<String>,
    pub api_base_url: Option<String>,
    pub account_id: Option<String>,
    pub user_id: Option<String>,
    #[serde(rename = "gateway_url")]
    pub lowcode_ws_base_url: Option<String>,
    pub lowcode_forward_enabled: Option<bool>,
    pub enabled: bool,
    pub connection: ConnectionStatus,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ConnectionStatus {
    pub running: bool,
    pub connected: bool,
    pub connected_at: Option<DateTime<Utc>>,
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    pub last_event_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub last_error_at: Option<DateTime<Utc>>,
    pub last_disconnect_at: Option<DateTime<Utc>>,
    pub last_disconnect_reason: Option<String>,
    pub reconnect_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReceivedEvent {
    pub received_at: DateTime<Utc>,
    pub event_type: String,
    pub message_type: Option<i64>,
    pub from_user_id: Option<String>,
    pub to_user_id: Option<String>,
    pub content: Option<String>,
    pub context_token: Option<String>,
    pub raw: Value,
}

#[derive(Debug, Deserialize)]
pub struct SendTextMessageRequest {
    #[serde(default, alias = "toUserId")]
    pub to_user_id: Option<String>,
    #[serde(default, alias = "contextToken")]
    pub context_token: Option<String>,
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct SendMediaMessageRequest {
    #[serde(default, alias = "toUserId")]
    pub to_user_id: Option<String>,
    #[serde(default, alias = "contextToken")]
    pub context_token: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default, alias = "mediaUrl")]
    pub media_url: Option<String>,
    #[serde(default, alias = "mediaPath")]
    pub media_path: Option<String>,
    #[serde(default, alias = "fileName")]
    pub file_name: Option<String>,
    #[serde(default, alias = "mediaType")]
    pub media_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SendSessionMessageRequest {
    pub session_key: String,
    #[serde(default, alias = "contextToken")]
    pub context_token: Option<String>,
    pub text: String,
}

pub fn mask_secret(secret: &str) -> String {
    if secret.len() <= 6 {
        return "******".to_string();
    }

    let prefix = &secret[..3];
    let suffix = &secret[secret.len() - 3..];
    format!("{prefix}******{suffix}")
}

pub fn generate_outbound_token() -> String {
    format!("wechat_outbound_{}", Uuid::new_v4().simple())
}
