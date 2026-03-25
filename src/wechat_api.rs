use crate::error::ServiceError;
use crate::models::{SendTextMessageRequest, TenantCredential};
use crate::state::{AppState, TenantContext};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use image::{DynamicImage, ImageFormat, Luma};
use qrcode::QrCode;
use rand::Rng as _;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Cursor;
use std::sync::Arc;
use tracing::info;

#[derive(Debug, Deserialize, Clone)]
pub struct LoginQrResponse {
    pub qrcode: String,
    #[serde(rename = "qrcode_img_content")]
    pub qrcode_img_content: String,
    pub ret: i64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LoginStatusResponse {
    pub status: String,
    #[serde(default)]
    pub bot_token: Option<String>,
    #[serde(default)]
    pub baseurl: Option<String>,
    #[serde(default, rename = "ilink_bot_id")]
    pub ilink_bot_id: Option<String>,
    #[serde(default, rename = "ilink_user_id")]
    pub ilink_user_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GetUpdatesResponse {
    #[serde(default)]
    pub ret: Option<i64>,
    #[serde(default)]
    pub errmsg: Option<String>,
    #[serde(default)]
    pub msgs: Vec<Value>,
    #[serde(default, rename = "get_updates_buf")]
    pub get_updates_buf: Option<String>,
}

fn normalize_base_url(value: &str) -> String {
    let trimmed = value.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        "https://ilinkai.weixin.qq.com".to_string()
    } else {
        trimmed.to_string()
    }
}

fn random_wechat_uin() -> String {
    let value = rand::rng().random::<u32>().to_string();
    BASE64_STANDARD.encode(value)
}

pub fn normalize_login_qr_image_data_url(value: &str) -> Result<String, ServiceError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ServiceError::Upstream(
            "微信未返回可用的二维码内容".to_string(),
        ));
    }

    if trimmed.starts_with("data:image/") {
        return Ok(trimmed.to_string());
    }

    if looks_like_base64(trimmed) {
        return Ok(format!("data:image/png;base64,{trimmed}"));
    }

    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return build_png_qr_data_url(trimmed);
    }

    Err(ServiceError::Upstream(format!(
        "未识别的二维码内容格式: {trimmed}"
    )))
}

fn looks_like_base64(value: &str) -> bool {
    !value.is_empty()
        && value.len().is_multiple_of(4)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
}

fn build_png_qr_data_url(content: &str) -> Result<String, ServiceError> {
    let qrcode = QrCode::new(content.as_bytes())
        .map_err(|err| ServiceError::Internal(format!("生成二维码失败: {err}")))?;
    let image = qrcode
        .render::<Luma<u8>>()
        .min_dimensions(280, 280)
        .quiet_zone(true)
        .build();
    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageLuma8(image)
        .write_to(&mut output, ImageFormat::Png)
        .map_err(|err| ServiceError::Internal(format!("编码二维码图片失败: {err}")))?;
    let encoded = BASE64_STANDARD.encode(output.into_inner());
    Ok(format!("data:image/png;base64,{encoded}"))
}

fn build_headers(token: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Content-Type", "application/json".to_string()),
        ("AuthorizationType", "ilink_bot_token".to_string()),
        ("Authorization", format!("Bearer {token}")),
        ("X-WECHAT-UIN", random_wechat_uin()),
    ]
}

async fn parse_api_json(resp: reqwest::Response, endpoint: &str) -> Result<Value, ServiceError> {
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|err| ServiceError::Upstream(format!("读取微信响应失败: {err}")))?;
    if !status.is_success() {
        return Err(ServiceError::Upstream(format!(
            "{endpoint} 返回 HTTP {}: {}",
            status.as_u16(),
            text
        )));
    }

    serde_json::from_str::<Value>(&text).map_err(|err| {
        ServiceError::Upstream(format!("解析微信响应 JSON 失败: {err}; body={text}"))
    })
}

fn mask_token(value: &str) -> String {
    let trimmed = value.trim();
    let len = trimmed.chars().count();
    if len <= 12 {
        return "******".to_string();
    }
    let prefix: String = trimmed.chars().take(6).collect();
    let suffix: String = trimmed
        .chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}******{suffix}")
}

fn redact_sendmessage_payload(payload: &Value) -> Value {
    let mut redacted = payload.clone();
    if let Some(value) = redacted.pointer_mut("/msg/context_token") {
        if let Some(token) = value.as_str() {
            *value = Value::String(mask_token(token));
        }
    }
    redacted
}

fn ensure_success(body: &Value, endpoint: &str) -> Result<(), ServiceError> {
    let ret = body.get("ret").and_then(Value::as_i64).unwrap_or(0);
    if ret == 0 {
        return Ok(());
    }

    let errmsg = body
        .get("errmsg")
        .and_then(Value::as_str)
        .unwrap_or("unknown error");
    Err(ServiceError::Upstream(format!(
        "{endpoint} 返回错误 ret={ret} errmsg={errmsg}"
    )))
}

fn ensure_tenant_active(credential: &TenantCredential) -> Result<(), ServiceError> {
    if !credential.is_enabled() {
        return Err(ServiceError::BadRequest("租户已停用".to_string()));
    }
    if !credential.is_logged_in() {
        return Err(ServiceError::BadRequest("租户尚未扫码登录".to_string()));
    }
    Ok(())
}

pub async fn fetch_login_qrcode(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    base_url_override: Option<&str>,
) -> Result<LoginQrResponse, ServiceError> {
    let tenant_base = tenant.credential.read().await.api_base_url.clone();
    let base_url = normalize_base_url(
        base_url_override
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or(tenant_base.as_deref())
            .unwrap_or(state.config.wechat.base_url.as_str()),
    );
    let endpoint = format!("{base_url}/ilink/bot/get_bot_qrcode?bot_type=3");

    let body = parse_api_json(
        state
            .http_client
            .get(&endpoint)
            .send()
            .await
            .map_err(|err| ServiceError::Upstream(format!("获取二维码失败: {err}")))?,
        "get_bot_qrcode",
    )
    .await?;
    ensure_success(&body, "get_bot_qrcode")?;

    serde_json::from_value(body)
        .map_err(|err| ServiceError::Upstream(format!("解析二维码响应失败: {err}")))
}

#[cfg(test)]
mod tests {
    use super::normalize_login_qr_image_data_url;

    #[test]
    fn keeps_existing_data_url() {
        let value = "data:image/png;base64,abc123";
        let normalized = normalize_login_qr_image_data_url(value).expect("normalize data url");
        assert_eq!(normalized, value);
    }

    #[test]
    fn wraps_plain_base64_as_png_data_url() {
        let normalized =
            normalize_login_qr_image_data_url("YWJjZA==").expect("normalize base64 payload");
        assert_eq!(normalized, "data:image/png;base64,YWJjZA==");
    }

    #[test]
    fn generates_png_data_url_from_login_page_url() {
        let normalized = normalize_login_qr_image_data_url(
            "https://liteapp.weixin.qq.com/q/7GiQu1?qrcode=test&bot_type=3",
        )
        .expect("normalize login url");
        assert!(normalized.starts_with("data:image/png;base64,"));
        assert!(normalized.len() > "data:image/png;base64,".len());
    }
}

pub async fn poll_login_status(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    qrcode: &str,
    base_url_override: Option<&str>,
) -> Result<LoginStatusResponse, ServiceError> {
    if qrcode.trim().is_empty() {
        return Err(ServiceError::BadRequest("qrcode 不能为空".to_string()));
    }

    let tenant_base = tenant.credential.read().await.api_base_url.clone();
    let base_url = normalize_base_url(
        base_url_override
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or(tenant_base.as_deref())
            .unwrap_or(state.config.wechat.base_url.as_str()),
    );
    let endpoint = format!("{base_url}/ilink/bot/get_qrcode_status");
    let body = parse_api_json(
        state
            .http_client
            .get(&endpoint)
            .query(&[("qrcode", qrcode)])
            .send()
            .await
            .map_err(|err| ServiceError::Upstream(format!("轮询扫码状态失败: {err}")))?,
        "get_qrcode_status",
    )
    .await?;

    serde_json::from_value(body)
        .map_err(|err| ServiceError::Upstream(format!("解析扫码状态失败: {err}")))
}

pub async fn get_updates(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    sync_buf: Option<&str>,
) -> Result<GetUpdatesResponse, ServiceError> {
    let credential = tenant.credential.read().await.clone();
    ensure_tenant_active(&credential)?;

    let token = credential
        .bot_token
        .as_deref()
        .ok_or_else(|| ServiceError::BadRequest("租户缺少 bot_token".to_string()))?;
    let base_url = normalize_base_url(
        credential
            .api_base_url
            .as_deref()
            .unwrap_or(state.config.wechat.base_url.as_str()),
    );
    let endpoint = format!("{base_url}/ilink/bot/getupdates");

    let body_str = json!({
        "get_updates_buf": sync_buf.unwrap_or_default(),
        "base_info": {
            "channel_version": "0.1.0"
        }
    });

    let mut request = state.http_client.post(&endpoint).json(&body_str);
    for (key, value) in build_headers(token) {
        request = request.header(key, value);
    }

    let body = parse_api_json(
        request
            .send()
            .await
            .map_err(|err| ServiceError::Upstream(format!("轮询微信消息失败: {err}")))?,
        "getupdates",
    )
    .await?;

    serde_json::from_value(body)
        .map_err(|err| ServiceError::Upstream(format!("解析 getupdates 响应失败: {err}")))
}

pub async fn send_text_message(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    req: SendTextMessageRequest,
) -> Result<Value, ServiceError> {
    let user_id = req
        .to_user_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ServiceError::BadRequest("toUserId/to_user_id 不能为空".to_string()))?;
    send_text_to_user(
        state,
        tenant,
        user_id,
        &req.text,
        req.context_token.as_deref(),
    )
    .await
}

pub async fn send_text_to_user(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    user_id: &str,
    text: &str,
    context_token: Option<&str>,
) -> Result<Value, ServiceError> {
    let credential = tenant.credential.read().await.clone();
    ensure_tenant_active(&credential)?;
    if text.trim().is_empty() {
        return Err(ServiceError::BadRequest("text 不能为空".to_string()));
    }

    let token = credential
        .bot_token
        .as_deref()
        .ok_or_else(|| ServiceError::BadRequest("租户缺少 bot_token".to_string()))?;
    let base_url = normalize_base_url(
        credential
            .api_base_url
            .as_deref()
            .unwrap_or(state.config.wechat.base_url.as_str()),
    );
    let context_token = if let Some(value) = context_token
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
    {
        value
    } else if let Some(value) = tenant.get_context_token(user_id).await {
        value
    } else {
        return Err(ServiceError::BadRequest(
            "缺少 contextToken，无法回消息".to_string(),
        ));
    };

    let endpoint = format!("{base_url}/ilink/bot/sendmessage");
    let body_str = json!({
        "msg": {
            "from_user_id": "",
            "to_user_id": user_id,
            "client_id": format!("lowcode-wechat-{}", uuid::Uuid::new_v4()),
            "message_type": 2,
            "message_state": 2,
            "item_list": [{
                "type": 1,
                "text_item": {
                    "text": text
                }
            }],
            "context_token": context_token
        },
        "base_info": {
            "channel_version": "0.1.0"
        }
    });

    let log_payload = redact_sendmessage_payload(&body_str);
    info!(
        tenant_id = %tenant.tenant_id,
        user_id,
        endpoint = %endpoint,
        payload = %log_payload,
        "调用微信 sendmessage 请求"
    );

    let mut request = state.http_client.post(&endpoint).json(&body_str);
    for (key, value) in build_headers(token) {
        request = request.header(key, value);
    }

    let response = request
        .send()
        .await
        .map_err(|err| ServiceError::Upstream(format!("发送微信消息失败: {err}")))?;
    let status = response.status();
    let raw_body = response
        .text()
        .await
        .map_err(|err| ServiceError::Upstream(format!("读取微信响应失败: {err}")))?;
    info!(
        tenant_id = %tenant.tenant_id,
        user_id,
        endpoint = %endpoint,
        status = %status,
        response_body = %raw_body,
        "微信 sendmessage 响应"
    );
    let body = if !status.is_success() {
        return Err(ServiceError::Upstream(format!(
            "sendmessage 返回 HTTP {}: {}",
            status.as_u16(),
            raw_body
        )));
    } else {
        serde_json::from_str::<Value>(&raw_body).map_err(|err| {
            ServiceError::Upstream(format!("解析微信响应 JSON 失败: {err}; body={raw_body}"))
        })?
    };
    ensure_success(&body, "sendmessage")?;
    Ok(body)
}

pub async fn send_session_text(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    session_key: &str,
    text: &str,
    context_token: Option<&str>,
) -> Result<Value, ServiceError> {
    let user_id = session_key_receiver(session_key)?;
    send_text_to_user(state, tenant, &user_id, text, context_token).await
}

pub fn session_key_receiver(session_key: &str) -> Result<String, ServiceError> {
    let trimmed = session_key.trim();
    let mut parts = trimmed.splitn(3, ':');
    let channel = parts.next().unwrap_or_default();
    let scope = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default().trim();

    if !channel.eq_ignore_ascii_case("wechat") {
        return Err(ServiceError::BadRequest(format!(
            "不支持的 sessionKey channel: {channel}"
        )));
    }
    if target.is_empty() {
        return Err(ServiceError::BadRequest(
            "sessionKey 缺少目标标识".to_string(),
        ));
    }

    match scope {
        "dm" | "user" => Ok(target.to_string()),
        _ => Err(ServiceError::BadRequest(format!(
            "不支持的 sessionKey scope: {scope}"
        ))),
    }
}
