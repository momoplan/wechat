use crate::error::ServiceError;
use crate::models::{SendSessionMessageRequest, SendTextMessageRequest};
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
    channel: Option<String>,
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

async fn handle_channel_callback(
    req: HttpRequest,
    payload: web::Json<ChannelCallbackRequest>,
    state: web::Data<Arc<AppState>>,
) -> Result<HttpResponse, ServiceError> {
    let expected_token = state
        .config
        .channel_gateway
        .inbound_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty());
    if !is_callback_authorized(&req, expected_token) {
        return Err(ServiceError::Unauthorized(
            "channel callback 鉴权失败".to_string(),
        ));
    }

    let payload = payload.into_inner();
    let channel = payload.channel.clone().or_else(|| {
        context_string(
            payload.reply_to.as_ref(),
            payload.metadata.as_ref(),
            &["channel"],
        )
    });
    if let Some(channel) = channel.as_deref() {
        if !channel.eq_ignore_ascii_case("wechat") {
            return Err(ServiceError::BadRequest(
                "channel/replyTo.channel/metadata.channel 必须为 wechat".to_string(),
            ));
        }
    }

    let session_key = payload.session_key.clone().or_else(|| {
        payload
            .session
            .as_ref()
            .and_then(|value| value_string(value, &["sessionKey", "session_key", "key"]))
    });
    let tenant_id = context_string(
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
    .ok_or_else(|| {
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

    let Some(text) = extract_reply_text(&payload.data) else {
        return Ok(HttpResponse::Ok().json(json!({
            "accepted": true,
            "delivered": false,
            "reason": "message_ignored"
        })));
    };

    let tenant = get_tenant(state.get_ref(), &tenant_id)?;
    let text = if let Some(bind_url) = extract_bind_url(&payload.data) {
        let bind_prompt = extract_bind_prompt(&payload.data).unwrap_or_else(|| text.clone());
        format!("{bind_prompt}\n{bind_url}")
    } else {
        text
    };
    let text = truncate_text(text, 1800);

    wechat_api::send_text_to_user(
        state.get_ref(),
        &tenant,
        &user_id,
        &text,
        context_token.as_deref(),
    )
    .await?;

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

fn is_callback_authorized(req: &HttpRequest, expected_token: Option<&str>) -> bool {
    let Some(expected) = expected_token else {
        return true;
    };

    if let Some(actual) = req
        .headers()
        .get("x-channel-token")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
    {
        if actual == expected {
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
        if actual == expected {
            return true;
        }
    }

    false
}

fn extract_reply_text(data: &Value) -> Option<String> {
    let message_type = data
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if data
        .get("streaming")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        return None;
    }
    if message_type.eq_ignore_ascii_case("agent-message-ack")
        || message_type.eq_ignore_ascii_case("session-created")
        || message_type.eq_ignore_ascii_case("complete")
        || message_type.contains("delta")
    {
        return None;
    }

    if let Some(text) = data.pointer("/content").and_then(extract_text_content) {
        return Some(text);
    }
    if let Some(text) = data
        .get("text")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(text.to_string());
    }
    if let Some(text) = data
        .get("content")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(text.to_string());
    }
    if let Some(text) = data
        .get("message")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(text.to_string());
    }
    if let Some(text) = data
        .pointer("/error/message")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(format!("请求失败: {}", text));
    }

    serde_json::to_string(data)
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty() && text != "{}")
}

fn extract_bind_url(data: &Value) -> Option<String> {
    data.pointer("/error/bindUrl")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn extract_bind_prompt(data: &Value) -> Option<String> {
    data.pointer("/error/displayMessage")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .or_else(|| {
            data.pointer("/error/message")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
        })
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
