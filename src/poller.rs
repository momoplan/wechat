use crate::config::CommandActionConfig;
use crate::models::ReceivedEvent;
use crate::state::{AppState, TenantContext};
use crate::wechat_api;
use aes::Aes128;
use aes::cipher::{BlockDecryptMut, KeyInit, block_padding::Pkcs7};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use chrono::Utc;
use ecb::Decryptor;
use hex::decode as hex_decode;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{error, info, warn};

const LOWCODE_FORWARD_FALLBACK_MESSAGE: &str = "消息暂时处理失败，请稍后重试或联系管理员。";
const WECHAT_CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";

type Aes128EcbDec = Decryptor<Aes128>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct InboundCommand {
    text: String,
    action: String,
    session_id: Option<String>,
    service: Option<String>,
    method: Option<String>,
    params: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionControlSuccess {
    action: String,
    session_id: Option<String>,
}

pub async fn run_tenant_poll_worker(
    state: Arc<AppState>,
    tenant: Arc<TenantContext>,
    mut stop_rx: watch::Receiver<bool>,
) {
    let retry_seconds = state.config.runtime.poll_retry_seconds.max(1);

    loop {
        if *stop_rx.borrow() {
            break;
        }

        tenant.mark_connecting().await;
        match poll_once(&state, &tenant, &mut stop_rx).await {
            Ok(()) => {
                if *stop_rx.borrow() {
                    break;
                }
                warn!(tenant_id = %tenant.tenant_id, "微信轮询已结束，准备重连");
            }
            Err(err) => {
                let error_text = err.to_string();
                tenant.mark_reconnect_pending(&error_text).await;
                tenant.set_last_error(&error_text).await;
                error!(tenant_id = %tenant.tenant_id, error = %err, "微信轮询处理失败");
            }
        }

        let sleep = tokio::time::sleep(Duration::from_secs(retry_seconds));
        tokio::pin!(sleep);
        tokio::select! {
            _ = &mut sleep => {}
            changed = stop_rx.changed() => {
                if changed.is_ok() && *stop_rx.borrow() {
                    break;
                }
            }
        }
    }

    tenant.refresh_runtime_flags().await;
    info!(tenant_id = %tenant.tenant_id, "租户微信轮询 worker 已停止");
}

async fn poll_once(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    stop_rx: &mut watch::Receiver<bool>,
) -> Result<(), anyhow::Error> {
    tenant.mark_connected().await;

    loop {
        tokio::select! {
            changed = stop_rx.changed() => {
                if changed.is_ok() && *stop_rx.borrow() {
                    return Ok(());
                }
            }
            result = poll_and_handle(state, tenant) => {
                result?;
            }
        }
    }
}

async fn poll_and_handle(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
) -> Result<(), anyhow::Error> {
    let sync_buf = tenant.credential.read().await.sync_buf.clone();
    let response = wechat_api::get_updates(state, tenant, sync_buf.as_deref()).await?;
    let ret = response.ret.unwrap_or(0);
    if ret != 0 {
        let errmsg = response
            .errmsg
            .unwrap_or_else(|| "unknown error".to_string());
        return Err(anyhow::anyhow!(
            "getupdates 返回错误 ret={} errmsg={}",
            ret,
            errmsg
        ));
    }

    tenant.mark_heartbeat().await;

    if let Some(next_buf) = response.get_updates_buf.as_deref() {
        state
            .update_sync_buf(&tenant.tenant_id, Some(next_buf))
            .await
            .map_err(anyhow::Error::from)?;
    }

    for msg in response.msgs {
        handle_inbound_message(state, tenant, msg).await;
    }

    Ok(())
}

async fn handle_inbound_message(state: &Arc<AppState>, tenant: &Arc<TenantContext>, msg: Value) {
    let message_type = msg.get("message_type").and_then(Value::as_i64);

    let from_user_id = msg
        .get("from_user_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let to_user_id = msg
        .get("to_user_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);

    let context_token = msg
        .get("context_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    if let (Some(from_user_id), Some(token)) = (from_user_id.as_deref(), context_token.as_deref()) {
        tenant.set_context_token(&from_user_id, token).await;
    }

    let content = extract_text(&msg);
    let message_type_value = message_type.unwrap_or_default();
    let redacted_raw = redact_inbound_message(&msg);
    info!(
        tenant_id = %tenant.tenant_id,
        message_type = message_type,
        from_user_id = ?from_user_id,
        to_user_id = ?to_user_id,
        context_token = context_token.as_deref().map(wechat_api::mask_identifier),
        content = %content,
        raw = %redacted_raw,
        "收到微信入站消息"
    );

    let event = ReceivedEvent {
        received_at: Utc::now(),
        event_type: "message".to_string(),
        message_type,
        from_user_id: from_user_id.clone(),
        to_user_id,
        content: Some(content.clone()),
        context_token: context_token.clone(),
        raw: msg.clone(),
    };

    tenant
        .push_event(event, state.config.runtime.event_retention_per_tenant)
        .await;

    let inbound_content = build_lowcode_inbound_content(&msg);
    let has_media = has_media_items(&msg);
    let command_actions = tenant
        .credential
        .read()
        .await
        .command_actions
        .clone()
        .unwrap_or_else(|| state.config.runtime.command_actions.clone());
    let assistant_name = tenant
        .credential
        .read()
        .await
        .assistant_name
        .clone()
        .unwrap_or_else(|| state.config.runtime.assistant_name.clone());
    let inbound_command = detect_inbound_command(
        message_type,
        &content,
        has_media,
        assistant_name.as_str(),
        &command_actions,
    );
    if message_type != Some(1) && !has_media {
        info!(
            tenant_id = %tenant.tenant_id,
            message_type = message_type_value,
            "微信入站消息未转发：当前仅处理文本消息"
        );
        return;
    }

    let Some(from_user_id) = from_user_id else {
        warn!(tenant_id = %tenant.tenant_id, raw = %redacted_raw, "微信入站文本消息缺少 from_user_id，已记录但不转发");
        return;
    };
    let Some(inbound_content) = inbound_content else {
        info!(
            tenant_id = %tenant.tenant_id,
            message_type = message_type_value,
            "微信入站消息未转发：未提取到可用内容"
        );
        return;
    };
    let inbound_content = inline_inbound_images(state, tenant, inbound_content).await;

    maybe_forward_to_lowcode_agent(
        state,
        tenant,
        &msg,
        &from_user_id,
        inbound_content,
        inbound_command,
        context_token.as_deref(),
    )
    .await;
}

fn extract_text(msg: &Value) -> String {
    let Some(items) = msg.get("item_list").and_then(Value::as_array) else {
        return "(empty message)".to_string();
    };

    let mut parts = Vec::new();
    for item in items {
        let item_type = item.get("type").and_then(Value::as_i64).unwrap_or_default();
        match item_type {
            1 => {
                if let Some(text) = item
                    .get("text_item")
                    .and_then(|value| value.get("text"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    parts.push(text.to_string());
                }
            }
            2 => parts.push("(image)".to_string()),
            3 => {
                if let Some(text) = item
                    .get("voice_item")
                    .and_then(|value| value.get("text"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    parts.push(text.to_string());
                } else {
                    parts.push("(voice)".to_string());
                }
            }
            4 => parts.push("(file)".to_string()),
            5 => parts.push("(video)".to_string()),
            _ => {}
        }
    }

    if parts.is_empty() {
        "(empty message)".to_string()
    } else {
        parts.join("\n")
    }
}

fn has_media_items(msg: &Value) -> bool {
    msg.get("item_list")
        .and_then(Value::as_array)
        .map(|items| {
            items.iter().any(|item| {
                matches!(
                    item.get("type").and_then(Value::as_i64),
                    Some(2) | Some(3) | Some(4) | Some(5)
                )
            })
        })
        .unwrap_or(false)
}

fn detect_inbound_command(
    message_type: Option<i64>,
    content: &str,
    has_media: bool,
    assistant_name: &str,
    command_actions: &[CommandActionConfig],
) -> Option<InboundCommand> {
    if message_type != Some(1) || has_media {
        return None;
    }

    let assistant_name = normalize_assistant_name(assistant_name)?;
    let text = normalize_command_text(content, &assistant_name)?;
    command_actions.iter().find_map(|item| {
        if text.eq_ignore_ascii_case(item.text.as_str()) {
            Some(InboundCommand {
                text: text.clone(),
                action: item.action.clone(),
                session_id: item.session_id.clone(),
                service: item.service.clone(),
                method: item.method.clone(),
                params: item.params.clone(),
            })
        } else {
            None
        }
    })
}

fn normalize_assistant_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn normalize_command_text(text: &str, assistant_name: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let without_prefix = trimmed.strip_prefix(assistant_name)?;
    let command = without_prefix
        .trim_start_matches(|ch: char| ch.is_whitespace() || matches!(ch, ',' | '，' | ':' | '：'))
        .trim();

    if command.is_empty() {
        None
    } else {
        Some(command.to_string())
    }
}

fn build_lowcode_inbound_content(msg: &Value) -> Option<Value> {
    let items = msg.get("item_list").and_then(Value::as_array)?;
    let mut parts = Vec::new();
    let mut has_text = false;
    let mut has_media = false;

    for item in items {
        let item_type = item.get("type").and_then(Value::as_i64).unwrap_or_default();
        match item_type {
            1 => {
                if let Some(text) = item
                    .get("text_item")
                    .and_then(|value| value.get("text"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    parts.push(json!({
                        "type": "text",
                        "text": text,
                    }));
                    has_text = true;
                }
            }
            2 => {
                parts.push(build_media_content_part(item, "image_url", "image"));
                has_media = true;
            }
            3 => {
                parts.push(build_media_content_part(item, "file", "audio"));
                has_media = true;
            }
            4 => {
                parts.push(build_media_content_part(item, "file", "file"));
                has_media = true;
            }
            5 => {
                parts.push(build_media_content_part(item, "file", "video"));
                has_media = true;
            }
            _ => {}
        }
    }

    if has_media && !has_text {
        let preview = extract_text(msg);
        if !preview.trim().is_empty() && preview != "(empty message)" {
            parts.insert(
                0,
                json!({
                    "type": "text",
                    "text": preview,
                }),
            );
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(Value::Array(parts))
    }
}

fn build_media_content_part(
    item: &Value,
    part_type: &str,
    media_kind: &str,
) -> Value {
    let detail_key = match media_kind {
        "image" => "image_item",
        "audio" => "voice_item",
        "video" => "video_item",
        "file" => "file_item",
        _ => "media_item",
    };
    let detail = item.get(detail_key).cloned().unwrap_or(Value::Null);
    let media = detail.get("media").cloned().unwrap_or(Value::Null);
    let download_url = media
        .get("encrypt_query_param")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(build_wechat_cdn_download_url);

    let mut part = json!({
        "type": part_type,
        "source": "wechat",
        "mediaType": media_kind,
        "wechatMedia": detail,
    });

    if let Some(object) = part.as_object_mut() {
        match part_type {
            "image_url" => {
                object.insert(
                    "image_url".to_string(),
                    download_url
                        .map(|url| json!({ "url": url }))
                        .unwrap_or(Value::Null),
                );
            }
            "file" => {
                object.insert(
                    "file".to_string(),
                    download_url
                        .map(|url| json!({ "url": url }))
                        .unwrap_or(Value::Null),
                );
            }
            _ => {}
        }
    }

    part
}

fn build_wechat_cdn_download_url(encrypted_query_param: &str) -> String {
    format!(
        "{WECHAT_CDN_BASE_URL}/download?encrypted_query_param={}",
        urlencoding::encode(encrypted_query_param)
    )
}

async fn inline_inbound_images(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    content: Value,
) -> Value {
    let Some(parts) = content.as_array() else {
        return content;
    };

    let max_inline_bytes = state.config.runtime.max_inline_image_bytes.max(1);
    let mut updated = Vec::with_capacity(parts.len());
    for part in parts {
        updated.push(inline_image_part(state, tenant, part.clone(), max_inline_bytes).await);
    }

    Value::Array(updated)
}

async fn inline_image_part(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    part: Value,
    max_inline_bytes: usize,
) -> Value {
    let is_image_part = part
        .get("type")
        .and_then(Value::as_str)
        .map(|value| {
            value.eq_ignore_ascii_case("image_url") || value.eq_ignore_ascii_case("input_image")
        })
        .unwrap_or(false);
    if !is_image_part {
        return part;
    }

    let Some(original_url) = part
        .get("image_url")
        .and_then(|value| match value {
            Value::String(text) => Some(text.as_str()),
            Value::Object(object) => object.get("url").and_then(Value::as_str),
            _ => None,
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
    else {
        return part;
    };

    match build_inline_image_data_url(state, &part, &original_url, max_inline_bytes).await {
        Ok(Some(data_url)) => {
            mark_image_part_transfer(part, Value::String(data_url), "inline_data_url")
        }
        Ok(None) => {
            warn!(
                tenant_id = %tenant.tenant_id,
                max_inline_bytes,
                "微信图片超过内联大小上限，继续透传原始链接"
            );
            mark_image_part_transfer(
                part,
                Value::String(original_url.clone()),
                "external_url_fallback",
            )
        }
        Err(err) => {
            warn!(
                tenant_id = %tenant.tenant_id,
                error = %err,
                "微信图片内联失败，继续透传原始链接"
            );
            mark_image_part_transfer(part, Value::String(original_url), "external_url_fallback")
        }
    }
}

fn mark_image_part_transfer(mut part: Value, image_url: Value, transfer: &str) -> Value {
    let Some(object) = part.as_object_mut() else {
        return part;
    };
    let normalized_image_url = match image_url {
        Value::String(url) => json!({ "url": url }),
        other => other,
    };
    object.insert("image_url".to_string(), normalized_image_url);
    object.insert(
        "imageTransfer".to_string(),
        Value::String(transfer.to_string()),
    );
    part
}

async fn build_inline_image_data_url(
    state: &Arc<AppState>,
    part: &Value,
    download_url: &str,
    max_inline_bytes: usize,
) -> Result<Option<String>, String> {
    let response = state
        .http_client
        .get(download_url)
        .send()
        .await
        .map_err(|err| format!("下载微信图片失败: {err}"))?;

    if let Some(length) = response.content_length()
        && length > max_inline_bytes as u64
    {
        return Ok(None);
    }

    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "(读取微信图片响应失败)".to_string());
        return Err(format!(
            "下载微信图片失败 HTTP {}: {}",
            status.as_u16(),
            body
        ));
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    let bytes = response
        .bytes()
        .await
        .map_err(|err| format!("读取微信图片失败: {err}"))?;
    let bytes = maybe_decrypt_wechat_media(part, bytes.as_ref())?;

    if bytes.len() > max_inline_bytes {
        return Ok(None);
    }

    let mime = resolve_image_mime_type(content_type.as_deref(), bytes.as_ref());
    Ok(Some(format!(
        "data:{mime};base64,{}",
        BASE64_STANDARD.encode(bytes)
    )))
}

fn maybe_decrypt_wechat_media(part: &Value, bytes: &[u8]) -> Result<Vec<u8>, String> {
    let Some(aes_key) = extract_wechat_media_aes_key(part)? else {
        return Ok(bytes.to_vec());
    };
    decrypt_aes_ecb(bytes, &aes_key)
}

fn extract_wechat_media_aes_key(part: &Value) -> Result<Option<[u8; 16]>, String> {
    let wechat_media = part.get("wechatMedia").unwrap_or(part);

    if let Some(hex_key) = wechat_media
        .get("aeskey")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return decode_wechat_aes_key_hex(hex_key).map(Some);
    }

    let Some(encoded_key) = wechat_media
        .get("media")
        .and_then(|value| value.get("aes_key"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };

    let decoded = BASE64_STANDARD
        .decode(encoded_key)
        .map_err(|err| format!("解析微信 media.aes_key 失败: {err}"))?;
    let decoded_str = std::str::from_utf8(&decoded)
        .map_err(|err| format!("微信 media.aes_key 不是合法 UTF-8: {err}"))?;
    decode_wechat_aes_key_hex(decoded_str).map(Some)
}

fn decode_wechat_aes_key_hex(hex_key: &str) -> Result<[u8; 16], String> {
    let decoded = hex_decode(hex_key).map_err(|err| format!("解析微信 aeskey 失败: {err}"))?;
    let len = decoded.len();
    decoded
        .try_into()
        .map_err(|_| format!("微信 aeskey 长度非法: {}", len))
}

fn decrypt_aes_ecb(ciphertext: &[u8], key: &[u8; 16]) -> Result<Vec<u8>, String> {
    let mut buffer = ciphertext.to_vec();
    let decryptor = Aes128EcbDec::new(key.into());
    let plaintext = decryptor
        .decrypt_padded_mut::<Pkcs7>(&mut buffer)
        .map_err(|err| format!("AES-128-ECB 解密失败: {err}"))?;
    Ok(plaintext.to_vec())
}

fn resolve_image_mime_type(content_type: Option<&str>, bytes: &[u8]) -> &'static str {
    if let Some(value) = content_type.and_then(normalize_image_content_type) {
        return value;
    }
    guess_image_mime_type(bytes).unwrap_or("image/png")
}

fn normalize_image_content_type(content_type: &str) -> Option<&'static str> {
    let mime = content_type
        .split(';')
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    match mime {
        "image/png" => Some("image/png"),
        "image/jpeg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        "image/bmp" => Some("image/bmp"),
        "image/svg+xml" => Some("image/svg+xml"),
        "image/x-icon" => Some("image/x-icon"),
        _ => None,
    }
}

fn guess_image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && bytes[8..12] == *b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

async fn maybe_forward_to_lowcode_agent(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    raw_message: &Value,
    user_id: &str,
    content: Value,
    inbound_command: Option<InboundCommand>,
    context_token: Option<&str>,
) {
    let credential = tenant.credential.read().await.clone();
    let forward_enabled = credential.lowcode_forward_enabled.unwrap_or(false);
    if !forward_enabled {
        info!(
            tenant_id = %tenant.tenant_id,
            user_id,
            "微信入站消息未转发：租户未启用 lowcode 转发"
        );
        return;
    }

    let Some(base_url) = credential
        .lowcode_ws_base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        warn!(
            tenant_id = %tenant.tenant_id,
            user_id,
            "微信入站消息未转发：租户未配置 gateway_url"
        );
        return;
    };

    let endpoint = build_lowcode_inbound_endpoint(base_url);
    let auth_token = credential
        .lowcode_ws_token
        .clone()
        .or_else(|| state.config.channel_gateway.inbound_token.clone());

    let data = match inbound_command.as_ref() {
        Some(command) => {
            info!(
                tenant_id = %tenant.tenant_id,
                user_id,
                action = %command.action,
                command = %command.text,
                "识别到会话控制命令，改发 session-control"
            );
            let mut payload = json!({
                "type": "session-control",
                "action": command.action,
                "source": "wechat-command",
                "tenantId": tenant.tenant_id.as_str(),
                "messageType": raw_message.get("message_type"),
                "fromUserId": user_id,
                "command": command.text,
                "sessionId": command.session_id,
                "content": content
            });
            if let Some(obj) = payload.as_object_mut() {
                if let Some(service) = command.service.as_ref() {
                    obj.insert("service".to_string(), json!(service));
                }
                if let Some(method) = command.method.as_ref() {
                    obj.insert("method".to_string(), json!(method));
                }
                if let Some(params) = command.params.as_ref() {
                    obj.insert("params".to_string(), params.clone());
                }
            }
            payload
        }
        None => json!({
            "type": "input",
            "source": "wechat",
            "tenantId": tenant.tenant_id.as_str(),
            "messageType": raw_message.get("message_type"),
            "fromUserId": user_id,
            "content": content
        }),
    };

    let body = json!({
        "requestId": raw_message.get("client_id").and_then(Value::as_str),
        "sender": {
            "channelUserId": user_id,
            "channelUserType": "wechat_user_id"
        },
        "session": {
            "scope": "dm",
            "userId": user_id,
            "sessionKey": format!("wechat:dm:{user_id}")
        },
        "replyTo": {
            "channel": "wechat",
            "tenantId": tenant.tenant_id.as_str(),
            "userId": user_id
        },
        "raw": raw_message,
        "data": data
    });

    let mut request = state.http_client.post(endpoint).json(&body);
    if let Some(token) = auth_token {
        request = request.bearer_auth(token);
    }

    match request.send().await {
        Ok(response) if response.status().is_success() => {
            let status = response.status();
            let response_text = response.text().await.unwrap_or_default();
            let response_body = serde_json::from_str::<Value>(&response_text).ok();
            info!(
                tenant_id = %tenant.tenant_id,
                user_id,
                status = %status,
                "微信消息已转发到 channel-gateway"
            );
            if let Some(command) = inbound_command.as_ref() {
                maybe_reply_session_control_success(
                    state,
                    tenant,
                    user_id,
                    context_token,
                    command,
                    response_body.as_ref(),
                )
                .await;
            }
        }
        Ok(response) => {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            warn!(
                tenant_id = %tenant.tenant_id,
                user_id,
                status = %status,
                body = %text,
                "转发 channel-gateway 失败"
            );
            reply_lowcode_forward_failure(
                state,
                tenant,
                user_id,
                context_token,
                Some(status.as_u16()),
            )
            .await;
        }
        Err(err) => {
            warn!(
                tenant_id = %tenant.tenant_id,
                user_id,
                error = %err,
                "调用 channel-gateway 失败"
            );
            reply_lowcode_forward_failure(state, tenant, user_id, context_token, None).await;
        }
    }
}

async fn maybe_reply_session_control_success(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    user_id: &str,
    context_token: Option<&str>,
    command: &InboundCommand,
    response_body: Option<&Value>,
) {
    let Some(reply_text) = session_control_success_reply_text(&command.action) else {
        return;
    };

    if let Some(success) = response_body.and_then(extract_session_control_success) {
        if !success.action.eq_ignore_ascii_case(&command.action) {
            return;
        }
    }

    if let Err(err) =
        wechat_api::send_text_to_user(state, tenant, user_id, reply_text, context_token).await
    {
        warn!(
            tenant_id = %tenant.tenant_id,
            user_id,
            action = %command.action,
            error = %err,
            "会话控制命令成功后发送确认消息失败"
        );
    }
}

fn redact_inbound_message(payload: &Value) -> Value {
    let mut redacted = payload.clone();
    if let Some(value) = redacted.pointer_mut("/context_token") {
        if let Some(token) = value.as_str() {
            *value = Value::String(wechat_api::mask_identifier(token));
        }
    }
    redacted
}

async fn reply_lowcode_forward_failure(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    user_id: &str,
    context_token: Option<&str>,
    status_code: Option<u16>,
) {
    if let Err(err) = wechat_api::send_text_to_user(
        state,
        tenant,
        user_id,
        LOWCODE_FORWARD_FALLBACK_MESSAGE,
        context_token,
    )
    .await
    {
        warn!(
            tenant_id = %tenant.tenant_id,
            user_id,
            status_code,
            error = %err,
            "lowcode 转发失败后回用户提示也失败"
        );
        tenant
            .set_last_error(format!("转发失败后回用户提示也失败: {err}"))
            .await;
        return;
    }

    info!(
        tenant_id = %tenant.tenant_id,
        user_id,
        status_code,
        "lowcode 转发失败，已向微信用户发送兜底提示"
    );
}

fn build_lowcode_inbound_endpoint(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.ends_with("/inbound") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/inbound")
    }
}

fn extract_session_control_success(payload: &Value) -> Option<SessionControlSuccess> {
    [
        Some(payload),
        payload.get("upstreamBody"),
        payload.get("upstream_body"),
        payload.get("data"),
        payload.pointer("/data/upstreamBody"),
        payload.pointer("/data/upstream_body"),
    ]
    .into_iter()
    .flatten()
    .find_map(parse_session_control_success)
}

fn parse_session_control_success(payload: &Value) -> Option<SessionControlSuccess> {
    let message_type = payload
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    if !message_type.eq_ignore_ascii_case("session-control") {
        return None;
    }

    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let session_id = payload
        .pointer("/result/sessionId")
        .or_else(|| payload.pointer("/result/session_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);

    Some(SessionControlSuccess {
        action: action.to_string(),
        session_id,
    })
}

fn session_control_success_reply_text(action: &str) -> Option<&'static str> {
    if action.eq_ignore_ascii_case("new") {
        Some("已开始新话题，可以继续发送消息了。")
    } else if action.eq_ignore_ascii_case("activate") {
        Some("已切换到指定会话，可以继续发送消息了。")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{
        InboundCommand, SessionControlSuccess, build_lowcode_inbound_content,
        build_wechat_cdn_download_url, decode_wechat_aes_key_hex, decrypt_aes_ecb,
        detect_inbound_command, extract_session_control_success, extract_wechat_media_aes_key,
        guess_image_mime_type, has_media_items, mark_image_part_transfer, normalize_assistant_name,
        normalize_command_text, normalize_image_content_type, session_control_success_reply_text,
    };
    use aes::Aes128;
    use aes::cipher::{BlockEncryptMut, KeyInit, block_padding::Pkcs7};
    use crate::config::CommandActionConfig;
    use ecb::Encryptor;
    use serde_json::Value;
    use serde_json::json;

    type Aes128EcbEnc = Encryptor<Aes128>;

    #[test]
    fn builds_text_only_content_array() {
        let msg = json!({
            "item_list": [
                {
                    "type": 1,
                    "text_item": {
                        "text": "hello"
                    }
                }
            ]
        });

        let content = build_lowcode_inbound_content(&msg).unwrap();
        assert_eq!(
            content,
            json!([
                {
                    "type": "text",
                    "text": "hello"
                }
            ])
        );
    }

    #[test]
    fn builds_image_content_with_placeholder_text_and_cdn_url() {
        let msg = json!({
            "item_list": [
                {
                    "type": 2,
                    "image_item": {
                        "media": {
                            "encrypt_query_param": "abc+/=",
                            "aes_key": "key"
                        },
                        "mid_size": 12
                    }
                }
            ]
        });

        let content = build_lowcode_inbound_content(&msg).unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "(image)");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(
            content[1]["image_url"]["url"],
            build_wechat_cdn_download_url("abc+/=")
        );
        assert_eq!(content[1]["wechatMedia"]["mid_size"], 12);
    }

    #[test]
    fn builds_video_content_and_detects_media_items() {
        let msg = json!({
            "item_list": [
                {
                    "type": 5,
                    "video_item": {
                        "media": {
                            "encrypt_query_param": "video-token"
                        },
                        "video_size": 1024
                    }
                }
            ]
        });

        assert!(has_media_items(&msg));
        let content = build_lowcode_inbound_content(&msg).unwrap();
        assert_eq!(content[0]["text"], "(video)");
        assert_eq!(content[1]["type"], "file");
        assert_eq!(content[1]["mediaType"], "video");
        assert_eq!(
            content[1]["file"]["url"],
            build_wechat_cdn_download_url("video-token")
        );
    }

    #[test]
    fn builds_voice_content_with_transcript_and_audio_file() {
        let msg = json!({
            "item_list": [
                {
                    "type": 3,
                    "voice_item": {
                        "text": "帮我再装一个小象萨斯",
                        "playtime": 3760,
                        "media": {
                            "encrypt_query_param": "voice-token",
                            "aes_key": "key"
                        }
                    }
                }
            ]
        });

        assert!(has_media_items(&msg));
        let content = build_lowcode_inbound_content(&msg).unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "帮我再装一个小象萨斯");
        assert_eq!(content[1]["type"], "file");
        assert_eq!(content[1]["mediaType"], "audio");
        assert_eq!(
            content[1]["file"]["url"],
            build_wechat_cdn_download_url("voice-token")
        );
        assert_eq!(content[1]["wechatMedia"]["playtime"], 3760);
    }

    #[test]
    fn normalizes_image_content_type_and_guesses_common_formats() {
        assert_eq!(
            normalize_image_content_type("image/png; charset=binary"),
            Some("image/png")
        );
        assert_eq!(
            normalize_image_content_type("application/octet-stream"),
            None
        );
        assert_eq!(
            guess_image_mime_type(b"\x89PNG\r\n\x1a\nrest"),
            Some("image/png")
        );
        assert_eq!(
            guess_image_mime_type(b"\xff\xd8\xffrest"),
            Some("image/jpeg")
        );
        assert_eq!(guess_image_mime_type(b"GIF89arest"), Some("image/gif"));
        assert_eq!(guess_image_mime_type(b"BMrest"), Some("image/bmp"));
        assert_eq!(
            guess_image_mime_type(b"RIFF1234WEBPrest"),
            Some("image/webp")
        );
    }

    #[test]
    fn marks_image_part_with_inline_transfer_metadata() {
        let part = json!({
            "type": "image_url",
            "image_url": {
                "url": "https://example.com/original.png"
            },
            "source": "wechat"
        });

        let updated = mark_image_part_transfer(
            part,
            Value::String("data:image/png;base64,AAAA".to_string()),
            "inline_data_url",
        );

        assert_eq!(
            updated["image_url"],
            json!({"url": "data:image/png;base64,AAAA"})
        );
        assert_eq!(updated["imageTransfer"], "inline_data_url");
        assert_eq!(updated["source"], "wechat");
    }

    #[test]
    fn extracts_wechat_aes_key_from_hex_or_base64_hex() {
        let expected: [u8; 16] = hex::decode("00112233445566778899aabbccddeeff")
            .unwrap()
            .try_into()
            .unwrap();

        let part = json!({
            "wechatMedia": {
                "aeskey": "00112233445566778899aabbccddeeff"
            }
        });
        assert_eq!(extract_wechat_media_aes_key(&part).unwrap(), Some(expected));

        let part = json!({
            "wechatMedia": {
                "media": {
                    "aes_key": "MDAxMTIyMzM0NDU1NjY3Nzg4OTlhYWJiY2NkZGVlZmY="
                }
            }
        });
        assert_eq!(extract_wechat_media_aes_key(&part).unwrap(), Some(expected));
    }

    #[test]
    fn decrypts_wechat_aes_ecb_ciphertext() {
        let plaintext = b"\x89PNG\r\n\x1a\nwechat-image";
        let key = decode_wechat_aes_key_hex("00112233445566778899aabbccddeeff").unwrap();
        let mut buffer = plaintext.to_vec();
        let original_len = buffer.len();
        let block_size = 16;
        let padded_len = ((original_len / block_size) + 1) * block_size;
        buffer.resize(padded_len, 0);
        let encryptor = Aes128EcbEnc::new((&key).into());
        let ciphertext = encryptor
            .encrypt_padded_mut::<Pkcs7>(&mut buffer, original_len)
            .unwrap()
            .to_vec();

        let decrypted = decrypt_aes_ecb(&ciphertext, &key).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn detects_new_session_command_from_text_message() {
        let commands = vec![
            CommandActionConfig {
                text: "新话题".to_string(),
                action: "new".to_string(),
                session_id: None,
                service: None,
                method: None,
                params: None,
            },
            CommandActionConfig {
                text: "开始新话题".to_string(),
                action: "new".to_string(),
                session_id: None,
                service: None,
                method: None,
                params: None,
            },
        ];
        assert_eq!(
            detect_inbound_command(Some(1), "小百 新话题", false, "小百", &commands),
            Some(InboundCommand {
                text: "新话题".to_string(),
                action: "new".to_string(),
                session_id: None,
                service: None,
                method: None,
                params: None,
            })
        );
        assert_eq!(
            detect_inbound_command(Some(1), " 小百，新话题 ", false, "小百", &commands),
            Some(InboundCommand {
                text: "新话题".to_string(),
                action: "new".to_string(),
                session_id: None,
                service: None,
                method: None,
                params: None,
            })
        );
    }

    #[test]
    fn ignores_non_command_or_non_text_messages() {
        let commands = vec![
            CommandActionConfig {
                text: "新话题".to_string(),
                action: "new".to_string(),
                session_id: None,
                service: None,
                method: None,
                params: None,
            },
            CommandActionConfig {
                text: "结束当前会话".to_string(),
                action: "abort".to_string(),
                session_id: None,
                service: None,
                method: None,
                params: None,
            },
        ];
        assert_eq!(
            detect_inbound_command(Some(1), "小百 新话题 帮我总结一下", false, "小百", &commands),
            None
        );
        assert_eq!(
            detect_inbound_command(Some(2), "小百 新话题", true, "小百", &commands),
            None
        );
    }

    #[test]
    fn detects_custom_configured_new_session_command() {
        let commands = vec![CommandActionConfig {
                text: "重置话题".to_string(),
                action: "new".to_string(),
                session_id: None,
                service: None,
                method: None,
                params: None,
            }];
        assert_eq!(
            detect_inbound_command(Some(1), " 小百 重置话题 ", false, "小百", &commands),
            Some(InboundCommand {
                text: "重置话题".to_string(),
                action: "new".to_string(),
                session_id: None,
                service: None,
                method: None,
                params: None,
            })
        );
        assert_eq!(
            detect_inbound_command(Some(1), "小百 新话题", false, "小百", &commands),
            None
        );
    }

    #[test]
    fn detects_custom_configured_abort_command() {
        let commands = vec![CommandActionConfig {
            text: "结束当前会话".to_string(),
            action: "abort".to_string(),
            session_id: None,
            service: None,
            method: None,
            params: None,
        }];
        assert_eq!(
            detect_inbound_command(Some(1), "小百 结束当前会话", false, "小百", &commands),
            Some(InboundCommand {
                text: "结束当前会话".to_string(),
                action: "abort".to_string(),
                session_id: None,
                service: None,
                method: None,
                params: None,
            })
        );
    }

    #[test]
    fn detects_custom_configured_activate_command() {
        let commands = vec![CommandActionConfig {
            text: "切回订单会话".to_string(),
            action: "activate".to_string(),
            session_id: Some("sess_order_123".to_string()),
            service: None,
            method: None,
            params: None,
        }];
        assert_eq!(
            detect_inbound_command(Some(1), "小百切回订单会话", false, "小百", &commands),
            Some(InboundCommand {
                text: "切回订单会话".to_string(),
                action: "activate".to_string(),
                session_id: Some("sess_order_123".to_string()),
                service: None,
                method: None,
                params: None,
            })
        );
    }

    #[test]
    fn detects_custom_configured_call_command() {
        let commands = vec![CommandActionConfig {
            text: "查询积分".to_string(),
            action: "call".to_string(),
            session_id: None,
            service: Some("crm-service".to_string()),
            method: Some("getPoints".to_string()),
            params: Some(json!({
                "scene": "wechat"
            })),
        }];
        assert_eq!(
            detect_inbound_command(Some(1), "小百：查询积分", false, "小百", &commands),
            Some(InboundCommand {
                text: "查询积分".to_string(),
                action: "call".to_string(),
                session_id: None,
                service: Some("crm-service".to_string()),
                method: Some("getPoints".to_string()),
                params: Some(json!({
                    "scene": "wechat"
                })),
            })
        );
    }

    #[test]
    fn normalizes_assistant_prefixed_command_text() {
        assert_eq!(normalize_assistant_name(" 小百 "), Some("小百".to_string()));
        assert_eq!(
            normalize_command_text("小百：新话题", "小百"),
            Some("新话题".to_string())
        );
        assert_eq!(
            normalize_command_text("小百新话题", "小百"),
            Some("新话题".to_string())
        );
        assert_eq!(normalize_command_text("新话题", "小百"), None);
        assert_eq!(normalize_command_text("小百", "小百"), None);
    }

    #[test]
    fn extracts_session_control_success_from_direct_response() {
        let payload = json!({
            "type": "session-control",
            "action": "new",
            "result": {
                "sessionId": "sess_123"
            }
        });

        assert_eq!(
            extract_session_control_success(&payload),
            Some(SessionControlSuccess {
                action: "new".to_string(),
                session_id: Some("sess_123".to_string()),
            })
        );
    }

    #[test]
    fn extracts_session_control_success_from_wrapped_response() {
        let payload = json!({
            "data": {
                "status": "accepted",
                "upstreamBody": {
                    "type": "session-control",
                    "action": "activate",
                    "result": {
                        "session_id": "sess_456"
                    }
                }
            }
        });

        assert_eq!(
            extract_session_control_success(&payload),
            Some(SessionControlSuccess {
                action: "activate".to_string(),
                session_id: Some("sess_456".to_string()),
            })
        );
    }

    #[test]
    fn returns_confirmation_text_for_supported_session_control_actions() {
        assert_eq!(
            session_control_success_reply_text("new"),
            Some("已开始新话题，可以继续发送消息了。")
        );
        assert_eq!(
            session_control_success_reply_text("activate"),
            Some("已切换到指定会话，可以继续发送消息了。")
        );
        assert_eq!(session_control_success_reply_text("abort"), None);
    }
}
