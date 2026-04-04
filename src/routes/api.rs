use crate::error::ServiceError;
use crate::models::{SendMediaMessageRequest, SendSessionMessageRequest, SendTextMessageRequest};
use crate::state::{AppState, TenantContext};
use crate::wechat_api;
use actix_web::{HttpRequest, HttpResponse, Scope, web};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Debug, Deserialize)]
struct TenantPath {
    tenant_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChannelCallbackRequest {
    #[serde(default)]
    event_id: Option<String>,
    #[serde(default)]
    event_type: Option<String>,
    #[serde(default)]
    session_key: Option<String>,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    session: Option<Value>,
    #[serde(default, alias = "replyTo", alias = "reply_to")]
    reply_to: Option<Value>,
    #[serde(default)]
    raw: Option<Value>,
    #[serde(default)]
    metadata: Option<Value>,
    data: Value,
}

#[derive(Debug, PartialEq, Eq)]
enum OutboundDelivery {
    Text(String),
    Media {
        text: Option<String>,
        media_url: String,
        media_type: Option<String>,
        file_name: Option<String>,
    },
}

pub fn scope() -> Scope {
    web::scope("")
        .service(super::compat::scope())
        .route("/health", web::get().to(health))
        .route(
            "/tenants/{tenant_id}/messages/session",
            web::post().to(send_session_message),
        )
        .route(
            "/tenants/{tenant_id}/messages/text",
            web::post().to(send_text_message),
        )
        .route(
            "/tenants/{tenant_id}/messages/media",
            web::post().to(send_media_message),
        )
        .route(
            "/internal/channel/callback",
            web::post().to(handle_channel_callback),
        )
        .route("/outbound", web::post().to(handle_channel_callback))
}

async fn health() -> HttpResponse {
    HttpResponse::Ok().json(json!({"status": "ok"}))
}

async fn send_session_message(
    path: web::Path<TenantPath>,
    payload: web::Json<SendSessionMessageRequest>,
    state: web::Data<Arc<AppState>>,
) -> Result<HttpResponse, ServiceError> {
    let tenant = get_tenant(state.get_ref(), &path.tenant_id)?;
    let req = payload.into_inner();

    if req.session_key.trim().is_empty() {
        return Err(ServiceError::BadRequest("sessionKey 不能为空".to_string()));
    }
    if req.text.trim().is_empty() {
        return Err(ServiceError::BadRequest("text 不能为空".to_string()));
    }

    let data = wechat_api::send_session_text(
        state.get_ref(),
        &tenant,
        &req.session_key,
        &req.text,
        req.context_token.as_deref(),
    )
    .await?;

    Ok(HttpResponse::Ok().json(json!({
        "tenant_id": path.tenant_id,
        "session_key": req.session_key,
        "data": data
    })))
}

async fn send_text_message(
    path: web::Path<TenantPath>,
    payload: web::Json<SendTextMessageRequest>,
    state: web::Data<Arc<AppState>>,
) -> Result<HttpResponse, ServiceError> {
    let tenant = get_tenant(state.get_ref(), &path.tenant_id)?;
    let data =
        wechat_api::send_text_message(state.get_ref(), &tenant, payload.into_inner()).await?;

    Ok(HttpResponse::Ok().json(json!({
        "tenant_id": path.tenant_id,
        "data": data
    })))
}

async fn send_media_message(
    path: web::Path<TenantPath>,
    payload: web::Json<SendMediaMessageRequest>,
    state: web::Data<Arc<AppState>>,
) -> Result<HttpResponse, ServiceError> {
    let tenant = get_tenant(state.get_ref(), &path.tenant_id)?;
    let data =
        wechat_api::send_media_message(state.get_ref(), &tenant, payload.into_inner()).await?;

    Ok(HttpResponse::Ok().json(json!({
        "tenant_id": path.tenant_id,
        "data": data
    })))
}

async fn handle_channel_callback(
    req: HttpRequest,
    payload: web::Json<ChannelCallbackRequest>,
    state: web::Data<Arc<AppState>>,
) -> Result<HttpResponse, ServiceError> {
    let payload = payload.into_inner();
    let expected_tokens = resolve_callback_auth_tokens(
        state.get_ref(),
        extract_callback_tenant_id(&payload).as_deref(),
    )
    .await;
    if !is_callback_authorized(&req, &expected_tokens) {
        return Err(ServiceError::Unauthorized(
            "channel callback 鉴权失败".to_string(),
        ));
    }

    let session_key = payload.session_key.clone().or_else(|| {
        payload
            .session
            .as_ref()
            .and_then(|value| value_string(value, &["sessionKey", "session_key", "key"]))
    });
    let tenant_id = extract_callback_tenant_id(&payload).ok_or_else(|| {
        ServiceError::BadRequest(
            "replyTo.tenantId/metadata.tenantId/session.tenantId 缺失".to_string(),
        )
    })?;
    let user_id = context_string(
        payload.reply_to.as_ref(),
        payload.metadata.as_ref(),
        &["userId", "user_id", "fromUserId", "from_user_id"],
    )
    .or_else(|| {
        payload
            .session
            .as_ref()
            .and_then(|value| value_string(value, &["userId", "user_id"]))
    })
    .or_else(|| {
        session_key
            .as_deref()
            .and_then(|value| wechat_api::session_key_receiver(value).ok())
    })
    .ok_or_else(|| {
        ServiceError::BadRequest(
            "replyTo.userId/metadata.userId/session.userId/sessionKey 缺失".to_string(),
        )
    })?;
    let context_token = context_string(
        payload.reply_to.as_ref(),
        payload.metadata.as_ref(),
        &["contextToken", "context_token"],
    )
    .or_else(|| {
        payload
            .raw
            .as_ref()
            .and_then(|value| value_string(value, &["contextToken", "context_token"]))
    });

    let Some(delivery) = extract_reply_delivery(&payload.data) else {
        return Ok(HttpResponse::Ok().json(json!({
            "accepted": true,
            "delivered": false,
            "reason": "message_ignored"
        })));
    };

    let tenant = get_tenant(state.get_ref(), &tenant_id)?;
    match delivery {
        OutboundDelivery::Text(text) => {
            let text = truncate_text(text, 1800);

            wechat_api::send_text_to_user(
                state.get_ref(),
                &tenant,
                &user_id,
                &text,
                context_token.as_deref(),
            )
            .await?;
        }
        OutboundDelivery::Media {
            text,
            media_url,
            media_type,
            file_name,
        } => {
            wechat_api::send_media_to_user(
                state.get_ref(),
                &tenant,
                &user_id,
                text.map(|value| truncate_text(value, 1800)),
                media_url,
                context_token.as_deref(),
                media_type,
                file_name,
            )
            .await?;
        }
    }

    Ok(HttpResponse::Ok().json(json!({
        "accepted": true,
        "delivered": true,
        "event_id": payload.event_id,
        "event_type": payload.event_type,
        "session_key": session_key,
        "request_id": payload.request_id,
        "session_present": payload.session.is_some(),
        "reply_to_present": payload.reply_to.is_some(),
        "metadata_present": payload.metadata.is_some(),
        "raw_present": payload.raw.is_some(),
        "tenant_id": tenant_id,
        "user_id": user_id
    })))
}

fn get_tenant(state: &Arc<AppState>, tenant_id: &str) -> Result<Arc<TenantContext>, ServiceError> {
    state
        .get_tenant(tenant_id)
        .ok_or_else(|| ServiceError::NotFound(format!("租户不存在: {tenant_id}")))
}

fn value_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    })
}

fn context_string(
    primary: Option<&Value>,
    secondary: Option<&Value>,
    keys: &[&str],
) -> Option<String> {
    primary
        .and_then(|value| value_string(value, keys))
        .or_else(|| secondary.and_then(|value| value_string(value, keys)))
}

async fn resolve_callback_auth_tokens(
    state: &Arc<AppState>,
    tenant_id: Option<&str>,
) -> Vec<String> {
    let mut tokens = Vec::with_capacity(2);

    if let Some(tenant_id) = tenant_id {
        if let Some(tenant) = state.get_tenant(tenant_id) {
            let credential = tenant.credential.read().await.clone();
            if let Some(token) = credential.outbound_token.and_then(|value| {
                let trimmed = value.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            }) {
                tokens.push(token);
            }
        }
    }

    if let Some(token) = state
        .config
        .channel_gateway
        .inbound_token
        .clone()
        .and_then(|value| {
            let trimmed = value.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        })
    {
        if !tokens.iter().any(|value| value == &token) {
            tokens.push(token);
        }
    }

    tokens
}

fn extract_callback_tenant_id(payload: &ChannelCallbackRequest) -> Option<String> {
    context_string(
        payload.reply_to.as_ref(),
        payload.metadata.as_ref(),
        &["tenantId", "tenant_id"],
    )
    .or_else(|| {
        payload
            .session
            .as_ref()
            .and_then(|value| value_string(value, &["tenantId", "tenant_id"]))
    })
}

fn is_callback_authorized(req: &HttpRequest, expected_tokens: &[String]) -> bool {
    if expected_tokens.is_empty() {
        return true;
    }

    if let Some(actual) = req
        .headers()
        .get("x-channel-token")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
    {
        if expected_tokens.iter().any(|expected| actual == expected) {
            return true;
        }
    }

    if let Some(actual) = req
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
    {
        if expected_tokens.iter().any(|expected| actual == expected) {
            return true;
        }
    }

    false
}

fn extract_reply_delivery(data: &Value) -> Option<OutboundDelivery> {
    let message_type = data
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if message_type.eq_ignore_ascii_case("agent-message-ack")
        || message_type.eq_ignore_ascii_case("session-created")
    {
        return None;
    }

    if let Some(delivery) = data
        .pointer("/result/output")
        .and_then(extract_content_delivery)
    {
        return Some(delivery);
    }
    if let Some(delivery) = data.get("content").and_then(extract_content_delivery) {
        return Some(delivery);
    }
    if let Some((media_url, media_type, file_name)) = extract_media_candidate(data) {
        return Some(OutboundDelivery::Media {
            text: extract_text_content(data),
            media_url,
            media_type,
            file_name,
        });
    }
    if let Some(text) = data
        .get("text")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(OutboundDelivery::Text(text.to_string()));
    }
    if let Some(text) = data
        .get("content")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(OutboundDelivery::Text(text.to_string()));
    }
    if let Some(text) = data
        .get("message")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(OutboundDelivery::Text(text.to_string()));
    }
    if let Some(text) = data
        .pointer("/error/message")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(OutboundDelivery::Text(format!("请求失败: {}", text)));
    }
    if let Some(text) = data
        .pointer("/result/error/message")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(OutboundDelivery::Text(format!("请求失败: {}", text)));
    }

    serde_json::to_string(data)
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty() && text != "{}")
        .map(OutboundDelivery::Text)
}

fn extract_content_delivery(value: &Value) -> Option<OutboundDelivery> {
    match value {
        Value::String(text) => normalize_text(text).map(OutboundDelivery::Text),
        Value::Array(items) => {
            let mut text_parts = Vec::new();
            let mut media: Option<(String, Option<String>, Option<String>)> = None;

            for item in items {
                if let Some(text) = extract_text_item(item) {
                    text_parts.push(text);
                }
                if media.is_none() {
                    media = extract_media_candidate(item);
                }
            }

            let text = if text_parts.is_empty() {
                None
            } else {
                Some(text_parts.join("\n\n"))
            };

            if let Some((media_url, media_type, file_name)) = media {
                Some(OutboundDelivery::Media {
                    text,
                    media_url,
                    media_type,
                    file_name,
                })
            } else {
                text.map(OutboundDelivery::Text)
            }
        }
        Value::Object(map) => {
            if let Some((media_url, media_type, file_name)) = extract_media_candidate(value) {
                let text = map
                    .get("text")
                    .and_then(extract_text_content)
                    .or_else(|| map.get("caption").and_then(extract_text_content))
                    .or_else(|| map.get("message").and_then(extract_text_content));
                return Some(OutboundDelivery::Media {
                    text,
                    media_url,
                    media_type,
                    file_name,
                });
            }

            map.get("text")
                .and_then(extract_text_content)
                .or_else(|| map.get("content").and_then(extract_text_content))
                .or_else(|| map.get("message").and_then(extract_text_content))
                .map(OutboundDelivery::Text)
        }
        _ => None,
    }
}

fn extract_text_item(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => {
            let item_type = map.get("type").and_then(Value::as_str).unwrap_or("");
            if item_type.eq_ignore_ascii_case("input_text")
                || item_type.eq_ignore_ascii_case("output_text")
                || item_type.eq_ignore_ascii_case("text")
            {
                map.get("text")
                    .and_then(extract_text_content)
                    .or_else(|| map.get("content").and_then(extract_text_content))
            } else {
                map.get("text")
                    .and_then(extract_text_content)
                    .or_else(|| map.get("content").and_then(extract_text_content))
            }
        }
        _ => extract_text_content(value),
    }
}

fn extract_media_candidate(value: &Value) -> Option<(String, Option<String>, Option<String>)> {
    let Value::Object(map) = value else {
        return None;
    };

    let direct = [
        ("imageUrl", Some("image")),
        ("image_url", Some("image")),
        ("audioUrl", Some("audio")),
        ("audio_url", Some("audio")),
        ("voiceUrl", Some("audio")),
        ("voice_url", Some("audio")),
        ("videoUrl", Some("video")),
        ("video_url", Some("video")),
        ("fileUrl", Some("file")),
        ("file_url", Some("file")),
        ("mediaUrl", None),
        ("media_url", None),
    ];

    for (key, kind) in direct {
        if let Some(url) = map
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let media_type = kind
                .map(ToString::to_string)
                .or_else(|| optional_media_type(map));
            return Some((url.to_string(), media_type, optional_file_name(map)));
        }
    }

    for (key, kind) in [
        ("image_url", Some("image")),
        ("audio_url", Some("audio")),
        ("voice_url", Some("audio")),
        ("video_url", Some("video")),
        ("file_url", Some("file")),
    ] {
        if let Some(url) = map
            .get(key)
            .and_then(|value| match value {
                Value::String(text) => Some(text.as_str()),
                Value::Object(object) => object.get("url").and_then(Value::as_str),
                _ => None,
            })
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some((
                url.to_string(),
                kind.map(ToString::to_string),
                optional_file_name(map),
            ));
        }
    }

    let item_type = map.get("type").and_then(Value::as_str).unwrap_or("");
    if (item_type.eq_ignore_ascii_case("image_url")
        || item_type.eq_ignore_ascii_case("input_image")
        || item_type.eq_ignore_ascii_case("audio_url")
        || item_type.eq_ignore_ascii_case("input_audio")
        || item_type.eq_ignore_ascii_case("voice_url")
        || item_type.eq_ignore_ascii_case("input_voice")
        || item_type.eq_ignore_ascii_case("video_url")
        || item_type.eq_ignore_ascii_case("file_url"))
        && let Some(url) = map
            .get("url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    {
        let media_type = if item_type.contains("image") {
            Some("image".to_string())
        } else if item_type.contains("audio") || item_type.contains("voice") {
            Some("audio".to_string())
        } else if item_type.contains("video") {
            Some("video".to_string())
        } else if item_type.contains("file") {
            Some("file".to_string())
        } else {
            optional_media_type(map)
        };
        return Some((url.to_string(), media_type, optional_file_name(map)));
    }

    None
}

fn optional_media_type(map: &serde_json::Map<String, Value>) -> Option<String> {
    map.get("mediaType")
        .or_else(|| map.get("media_type"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
}

fn optional_file_name(map: &serde_json::Map<String, Value>) -> Option<String> {
    map.get("fileName")
        .or_else(|| map.get("file_name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn extract_text_content(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => normalize_text(text),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().filter_map(extract_text_content).collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n\n"))
            }
        }
        Value::Object(map) => map
            .get("text")
            .and_then(extract_text_content)
            .or_else(|| map.get("content").and_then(extract_text_content))
            .or_else(|| map.get("message").and_then(extract_text_content)),
        _ => None,
    }
}

fn normalize_text(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

fn truncate_text(text: String, limit: usize) -> String {
    if text.chars().count() <= limit {
        text
    } else {
        text.chars().take(limit).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{OutboundDelivery, extract_reply_delivery};
    use serde_json::json;

    #[test]
    fn extract_reply_text_supports_plain_text_links() {
        let data = json!({
            "type": "agentMessage",
            "content": {
                "text": "请打开 https://example.com/path?a=1"
            }
        });

        assert_eq!(
            extract_reply_delivery(&data),
            Some(OutboundDelivery::Text(
                "请打开 https://example.com/path?a=1".to_string()
            ))
        );
    }

    #[test]
    fn extract_reply_text_ignores_ack_messages_only() {
        let ack = json!({
            "type": "agent-message-ack",
            "content": {"text": "ok"}
        });

        assert_eq!(extract_reply_delivery(&ack), None);
    }

    #[test]
    fn extract_reply_text_preserves_tool_and_stream_payloads() {
        let streaming = json!({
            "type": "message",
            "streaming": true,
            "content": {"text": "partial"}
        });
        let tool_call = json!({
            "type": "tool-call",
            "toolName": "dispatch_subagent"
        });

        assert_eq!(
            extract_reply_delivery(&streaming),
            Some(OutboundDelivery::Text("partial".to_string()))
        );
        let extracted = extract_reply_delivery(&tool_call).expect("should fallback to json");
        match extracted {
            OutboundDelivery::Text(text) => assert!(text.contains("tool-call")),
            other => panic!("unexpected delivery: {other:?}"),
        }
    }

    #[test]
    fn extract_reply_text_reads_complete_result_output() {
        let data = json!({
            "type": "complete",
            "result": {
                "status": "completed",
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {"type": "output_text", "text": "最终结果"}
                    ]
                }]
            }
        });

        assert_eq!(
            extract_reply_delivery(&data),
            Some(OutboundDelivery::Text("最终结果".to_string()))
        );
    }

    #[test]
    fn extract_reply_delivery_reads_agent_error_content() {
        let data = json!({
            "type": "agent-error",
            "content": [
                {"type": "text", "text": "读取个人飞书文档前需要先授权。"},
                {"type": "text", "text": "授权链接：https://example.com/oauth"}
            ],
            "error": {
                "code": "USER_NOT_BOUND"
            }
        });

        assert_eq!(
            extract_reply_delivery(&data),
            Some(OutboundDelivery::Text(
                "读取个人飞书文档前需要先授权。\n\n授权链接：https://example.com/oauth"
                    .to_string()
            ))
        );
    }

    #[test]
    fn extract_reply_delivery_extracts_media_payload() {
        let data = json!({
            "type": "agentMessage",
            "content": {
                "imageUrl": "https://example.com/a.png"
            }
        });

        assert_eq!(
            extract_reply_delivery(&data),
            Some(OutboundDelivery::Media {
                text: None,
                media_url: "https://example.com/a.png".to_string(),
                media_type: Some("image".to_string()),
                file_name: None,
            })
        );
    }

    #[test]
    fn extract_reply_delivery_extracts_text_and_media_from_array() {
        let data = json!({
            "type": "agentMessage",
            "content": [
                {"type": "input_text", "text": "图片如下"},
                {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}
            ]
        });

        assert_eq!(
            extract_reply_delivery(&data),
            Some(OutboundDelivery::Media {
                text: Some("图片如下".to_string()),
                media_url: "https://example.com/a.png".to_string(),
                media_type: Some("image".to_string()),
                file_name: None,
            })
        );
    }

    #[test]
    fn extract_reply_delivery_extracts_audio_payload() {
        let data = json!({
            "type": "agentMessage",
            "content": {
                "audioUrl": "https://example.com/a.wav",
                "text": "语音回复"
            }
        });

        assert_eq!(
            extract_reply_delivery(&data),
            Some(OutboundDelivery::Media {
                text: Some("语音回复".to_string()),
                media_url: "https://example.com/a.wav".to_string(),
                media_type: Some("audio".to_string()),
                file_name: None,
            })
        );
    }

    #[test]
    fn extract_reply_delivery_extracts_voice_item_from_array() {
        let data = json!({
            "type": "agentMessage",
            "content": [
                {"type": "text", "text": "请听语音"},
                {"type": "input_voice", "url": "https://example.com/a.amr"}
            ]
        });

        assert_eq!(
            extract_reply_delivery(&data),
            Some(OutboundDelivery::Media {
                text: Some("请听语音".to_string()),
                media_url: "https://example.com/a.amr".to_string(),
                media_type: Some("audio".to_string()),
                file_name: None,
            })
        );
    }
}
