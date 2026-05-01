use crate::channel_binding_client;
use crate::channel_session_client::{self, ChannelSessionRecord};
use crate::config::CommandActionConfig;
use crate::models::ReceivedEvent;
use crate::project_file_client;
use crate::state::{AppState, TenantContext};
use crate::wechat_api;
use aes::Aes128;
use aes::cipher::{BlockDecryptMut, KeyInit, block_padding::Pkcs7};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use chrono::{Local, TimeZone, Utc};
use ecb::Decryptor;
use hex::decode as hex_decode;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{error, info, warn};

const LOWCODE_FORWARD_FALLBACK_MESSAGE: &str = "消息暂时处理失败，请稍后重试或联系管理员。";
const LOWCODE_FORWARD_PAYLOAD_TOO_LARGE_MESSAGE: &str =
    "这张图片太大，当前无法直接处理。请先压缩图片后重试，或改为发送图片链接。";
const WECHAT_CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";
const ACTIVATE_RECENT_ACTION: &str = "activate_recent";
// Base64 内联会把图片体积放大约 33%，这里额外留一部分 JSON 包体余量，
// 避免在上游链路较紧的 body limit 下把 /inbound 请求直接撑爆。
const MAX_TRANSPORT_INLINE_IMAGE_BYTES: usize = 80 * 1024;
const SESSION_LIST_COMMAND_LIMIT: usize = 10;

type Aes128EcbDec = Decryptor<Aes128>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct InboundCommand {
    text: String,
    action: String,
    agent_config_id: Option<String>,
    session_id: Option<String>,
    recent_session_index: Option<usize>,
    service: Option<String>,
    method: Option<String>,
    params: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionControlSuccess {
    action: String,
    session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DownloadedImageData {
    bytes: Vec<u8>,
    mime: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DownloadedMediaData {
    bytes: Vec<u8>,
    mime: String,
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
    let command_actions = match tenant.credential.read().await.command_actions.clone() {
        Some(items) => items,
        None => state.effective_default_command_actions().await,
    };
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
    let inbound_content =
        normalize_inbound_media_parts(state, tenant, &from_user_id, inbound_content).await;

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
    if let Some(command) = command_actions.iter().find_map(|item| {
        if text.eq_ignore_ascii_case(item.text.as_str()) {
            Some(InboundCommand {
                text: text.clone(),
                action: item.action.clone(),
                agent_config_id: item.agent_config_id.clone(),
                session_id: item.session_id.clone(),
                recent_session_index: None,
                service: item.service.clone(),
                method: item.method.clone(),
                params: item.params.clone(),
            })
        } else {
            None
        }
    }) {
        return Some(command);
    }

    parse_recent_session_index(&text).map(|index| InboundCommand {
        text,
        action: ACTIVATE_RECENT_ACTION.to_string(),
        agent_config_id: None,
        session_id: None,
        recent_session_index: Some(index),
        service: None,
        method: None,
        params: None,
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

fn parse_recent_session_index(text: &str) -> Option<usize> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let index = trimmed.parse::<usize>().ok()?;
    (index > 0).then_some(index)
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

fn build_media_content_part(item: &Value, part_type: &str, media_kind: &str) -> Value {
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

async fn normalize_inbound_media_parts(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    channel_user_id: &str,
    content: Value,
) -> Value {
    let Some(parts) = content.as_array() else {
        return content;
    };

    let mut updated = Vec::with_capacity(parts.len());
    for part in parts {
        updated.push(normalize_inbound_media_part(state, tenant, channel_user_id, part.clone()).await);
    }

    Value::Array(updated)
}

fn effective_inline_image_max_bytes(configured_max: usize) -> usize {
    configured_max.max(1).min(MAX_TRANSPORT_INLINE_IMAGE_BYTES)
}

async fn normalize_inbound_media_part(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    channel_user_id: &str,
    part: Value,
) -> Value {
    let Some(media_descriptor) = classify_inbound_media_part(&part) else {
        return part;
    };

    if media_descriptor.kind == "image" {
        let max_inline_bytes =
            effective_inline_image_max_bytes(state.config.runtime.max_inline_image_bytes);
        return normalize_inbound_image_part(
            state,
            tenant,
            channel_user_id,
            part,
            &media_descriptor.url,
            max_inline_bytes,
        )
        .await;
    }

    normalize_inbound_file_part(
        state,
        tenant,
        channel_user_id,
        part,
        &media_descriptor.url,
        media_descriptor.kind,
    )
    .await
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InboundMediaDescriptor<'a> {
    kind: &'a str,
    url: String,
}

fn classify_inbound_media_part(part: &Value) -> Option<InboundMediaDescriptor<'static>> {
    let part_type = part
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    let media_kind = part
        .get("mediaType")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();

    let url_key = if part_type.eq_ignore_ascii_case("image_url")
        || part_type.eq_ignore_ascii_case("input_image")
    {
        Some("image_url")
    } else if part_type.eq_ignore_ascii_case("file")
        || part_type.eq_ignore_ascii_case("file_url")
        || part_type.eq_ignore_ascii_case("input_file")
        || part_type.eq_ignore_ascii_case("audio_url")
        || part_type.eq_ignore_ascii_case("input_audio")
        || part_type.eq_ignore_ascii_case("voice_url")
        || part_type.eq_ignore_ascii_case("input_voice")
        || part_type.eq_ignore_ascii_case("video_url")
    {
        Some("file")
    } else {
        None
    }?;

    let kind = if media_kind.eq_ignore_ascii_case("image") || url_key == "image_url" {
        "image"
    } else if media_kind.eq_ignore_ascii_case("audio") {
        "audio"
    } else if media_kind.eq_ignore_ascii_case("video") {
        "video"
    } else {
        "file"
    };

    let url = extract_part_url(part, url_key)?;
    Some(InboundMediaDescriptor { kind, url })
}

fn extract_part_url(part: &Value, key: &str) -> Option<String> {
    part.get(key)
        .and_then(|value| match value {
            Value::String(text) => Some(text.as_str()),
            Value::Object(object) => object.get("url").and_then(Value::as_str),
            _ => None,
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

async fn normalize_inbound_image_part(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    channel_user_id: &str,
    part: Value,
    original_url: &str,
    max_inline_bytes: usize,
) -> Value {
    let original_url = original_url.to_string();

    match download_wechat_image_data(state, &part, &original_url).await {
        Ok(downloaded) if downloaded.bytes.len() <= max_inline_bytes => {
            replace_image_part_url(part, build_image_data_url(&downloaded))
        }
        Ok(downloaded) => match upload_wechat_image_part(
            state,
            tenant,
            channel_user_id,
            &part,
            &original_url,
            &downloaded,
        )
        .await
        {
            Ok(uploaded_url) => replace_image_part_url(part, uploaded_url),
            Err(err) => {
                warn!(
                    tenant_id = %tenant.tenant_id,
                    error = %err,
                    max_inline_bytes,
                    "微信图片超过内联大小上限，上传失败，保留原始远端地址"
                );
                replace_image_part_url(part, original_url.clone())
            }
        },
        Err(err) if err == "IMAGE_TOO_LARGE_BY_CONTENT_LENGTH" => {
            warn!(
                tenant_id = %tenant.tenant_id,
                max_inline_bytes,
                "微信图片响应头已超过内联大小上限，改走上传"
            );
            match download_wechat_image_data_force(state, &part, &original_url).await {
                Ok(downloaded) => match upload_wechat_image_part(
                    state,
                    tenant,
                    channel_user_id,
                    &part,
                    &original_url,
                    &downloaded,
                )
                .await
                {
                    Ok(uploaded_url) => replace_image_part_url(part, uploaded_url),
                    Err(upload_err) => {
                        warn!(
                            tenant_id = %tenant.tenant_id,
                            error = %upload_err,
                            "微信图片上传失败，保留原始远端地址"
                        );
                        replace_image_part_url(part, original_url.clone())
                    }
                },
                Err(force_err) => {
                    warn!(
                        tenant_id = %tenant.tenant_id,
                        error = %force_err,
                        "微信图片重新下载失败，保留原始远端地址"
                    );
                    replace_image_part_url(part, original_url.clone())
                }
            }
        }
        Err(err) => {
            warn!(
                tenant_id = %tenant.tenant_id,
                error = %err,
                "微信图片处理失败，保留原始远端地址"
            );
            replace_image_part_url(part, original_url)
        }
    }
}

async fn normalize_inbound_file_part(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    channel_user_id: &str,
    part: Value,
    original_url: &str,
    media_kind: &str,
) -> Value {
    let original_url = original_url.to_string();
    match download_wechat_media_data(state, &part, &original_url, media_kind).await {
        Ok(downloaded) => match upload_wechat_file_part(
            state,
            tenant,
            channel_user_id,
            &part,
            &original_url,
            media_kind,
            &downloaded,
        )
        .await
        {
            Ok(uploaded_url) => replace_file_part_url(part, uploaded_url),
            Err(err) => {
                warn!(
                    tenant_id = %tenant.tenant_id,
                    error = %err,
                    media_kind,
                    "微信文件上传失败，保留原始远端地址"
                );
                replace_file_part_url(part, original_url)
            }
        },
        Err(err) => {
            warn!(
                tenant_id = %tenant.tenant_id,
                error = %err,
                media_kind,
                "微信文件处理失败，保留原始远端地址"
            );
            replace_file_part_url(part, original_url)
        }
    }
}

fn replace_image_part_url(mut part: Value, image_url: impl Into<String>) -> Value {
    let Some(object) = part.as_object_mut() else {
        return part;
    };
    object.insert("image_url".to_string(), json!({ "url": image_url.into() }));
    part
}

fn replace_file_part_url(mut part: Value, file_url: impl Into<String>) -> Value {
    let Some(object) = part.as_object_mut() else {
        return part;
    };
    object.insert("file".to_string(), json!({ "url": file_url.into() }));
    part
}

async fn download_wechat_image_data(
    state: &Arc<AppState>,
    part: &Value,
    download_url: &str,
) -> Result<DownloadedImageData, String> {
    let response = state
        .http_client
        .get(download_url)
        .send()
        .await
        .map_err(|err| format!("下载微信图片失败: {err}"))?;

    if let Some(length) = response.content_length()
        && length > MAX_TRANSPORT_INLINE_IMAGE_BYTES as u64
    {
        return Err("IMAGE_TOO_LARGE_BY_CONTENT_LENGTH".to_string());
    }
    finalize_wechat_image_download(part, response).await
}

async fn download_wechat_media_data(
    state: &Arc<AppState>,
    part: &Value,
    download_url: &str,
    media_kind: &str,
) -> Result<DownloadedMediaData, String> {
    let response = state
        .http_client
        .get(download_url)
        .send()
        .await
        .map_err(|err| format!("下载微信文件失败: {err}"))?;

    finalize_wechat_media_download(part, response, media_kind).await
}

async fn download_wechat_image_data_force(
    state: &Arc<AppState>,
    part: &Value,
    download_url: &str,
) -> Result<DownloadedImageData, String> {
    let response = state
        .http_client
        .get(download_url)
        .send()
        .await
        .map_err(|err| format!("下载微信图片失败: {err}"))?;
    finalize_wechat_image_download(part, response).await
}

async fn finalize_wechat_image_download(
    part: &Value,
    response: reqwest::Response,
) -> Result<DownloadedImageData, String> {
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
    let mime = resolve_image_mime_type(content_type.as_deref(), bytes.as_ref());
    Ok(DownloadedImageData { bytes, mime })
}

async fn finalize_wechat_media_download(
    part: &Value,
    response: reqwest::Response,
    media_kind: &str,
) -> Result<DownloadedMediaData, String> {
    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "(读取微信文件响应失败)".to_string());
        return Err(format!(
            "下载微信文件失败 HTTP {}: {}",
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
        .map_err(|err| format!("读取微信文件失败: {err}"))?;
    let bytes = maybe_decrypt_wechat_media(part, bytes.as_ref())?;
    let mime = resolve_media_mime_type(content_type.as_deref(), bytes.as_ref(), media_kind);
    Ok(DownloadedMediaData { bytes, mime })
}

fn build_image_data_url(image: &DownloadedImageData) -> String {
    format!(
        "data:{};base64,{}",
        image.mime,
        BASE64_STANDARD.encode(&image.bytes)
    )
}

async fn upload_wechat_image_part(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    channel_user_id: &str,
    part: &Value,
    original_url: &str,
    downloaded: &DownloadedImageData,
) -> Result<String, String> {
    let binding = channel_binding_client::get_channel_user_binding(state, tenant, channel_user_id)
        .await
        .map_err(|err| err.to_string())?;
    let file_name = build_image_file_name(part, original_url, downloaded.mime);
    let uploaded = project_file_client::upload_image_bytes(
        state,
        tenant,
        &binding.workspace_id,
        channel_user_id,
        &file_name,
        downloaded.mime,
        &downloaded.bytes,
    )
    .await
    .map_err(|err| err.to_string())?;
    Ok(uploaded.download_url)
}

async fn upload_wechat_file_part(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    channel_user_id: &str,
    part: &Value,
    original_url: &str,
    media_kind: &str,
    downloaded: &DownloadedMediaData,
) -> Result<String, String> {
    let binding = channel_binding_client::get_channel_user_binding(state, tenant, channel_user_id)
        .await
        .map_err(|err| err.to_string())?;
    let file_name = build_media_file_name(part, original_url, &downloaded.mime, media_kind);
    let uploaded = project_file_client::upload_bytes(
        state,
        tenant,
        &binding.workspace_id,
        channel_user_id,
        &file_name,
        &downloaded.mime,
        &downloaded.bytes,
    )
    .await
    .map_err(|err| err.to_string())?;
    Ok(uploaded.download_url)
}

fn build_image_file_name(part: &Value, original_url: &str, mime: &str) -> String {
    build_media_file_name(part, original_url, mime, "image")
}

fn build_media_file_name(
    part: &Value,
    original_url: &str,
    mime: &str,
    media_kind: &str,
) -> String {
    let base = part
        .get("wechatMedia")
        .and_then(Value::as_object)
        .and_then(|media| media.get("file_name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .or_else(|| {
            original_url
                .split('?')
                .next()
                .and_then(|value| value.rsplit('/').next())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| format!("wechat_{media_kind}"));
    if base.contains('.') {
        base
    } else {
        format!("{base}.{}", guess_extension_from_mime(mime))
    }
}

fn guess_extension_from_mime(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/svg+xml" => "svg",
        "image/x-icon" => "ico",
        "audio/amr" => "amr",
        "audio/wav" => "wav",
        "audio/x-wav" => "wav",
        "audio/mpeg" => "mp3",
        "audio/mp4" => "m4a",
        "audio/aac" => "aac",
        "audio/ogg" => "ogg",
        "audio/opus" => "opus",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        "application/pdf" => "pdf",
        _ => "bin",
    }
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

fn resolve_media_mime_type(content_type: Option<&str>, bytes: &[u8], media_kind: &str) -> String {
    if let Some(value) = normalize_media_content_type(content_type, media_kind) {
        return value.to_string();
    }

    if media_kind.eq_ignore_ascii_case("image") {
        return resolve_image_mime_type(content_type, bytes).to_string();
    }

    if media_kind.eq_ignore_ascii_case("audio") && looks_like_amr(bytes) {
        return "audio/amr".to_string();
    }

    if media_kind.eq_ignore_ascii_case("video") && looks_like_mp4(bytes) {
        return "video/mp4".to_string();
    }

    "application/octet-stream".to_string()
}

fn normalize_media_content_type(content_type: Option<&str>, media_kind: &str) -> Option<&'static str> {
    let mime = content_type
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .split(';')
        .next()
        .map(str::trim)
        .unwrap_or_default();

    match mime {
        "image/png" => Some("image/png"),
        "image/jpeg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        "image/bmp" => Some("image/bmp"),
        "image/svg+xml" => Some("image/svg+xml"),
        "image/x-icon" => Some("image/x-icon"),
        "audio/amr" | "audio/amr-wb" | "application/amr" => Some("audio/amr"),
        "audio/wav" | "audio/x-wav" => Some("audio/wav"),
        "audio/mpeg" => Some("audio/mpeg"),
        "audio/mp4" => Some("audio/mp4"),
        "audio/aac" => Some("audio/aac"),
        "audio/ogg" => Some("audio/ogg"),
        "audio/opus" => Some("audio/opus"),
        "video/mp4" => Some("video/mp4"),
        "video/webm" => Some("video/webm"),
        "video/quicktime" => Some("video/quicktime"),
        "application/pdf" => Some("application/pdf"),
        "application/octet-stream" => {
            if media_kind.eq_ignore_ascii_case("file") {
                Some("application/octet-stream")
            } else {
                None
            }
        }
        _ => None,
    }
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

fn looks_like_amr(bytes: &[u8]) -> bool {
    bytes.starts_with(b"#!AMR")
}

fn looks_like_mp4(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && bytes[4..8] == *b"ftyp"
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
    let inbound_command = if let Some(command) = inbound_command {
        if command.action.eq_ignore_ascii_case("list_commands") {
            handle_local_list_commands_command(state, tenant, user_id, context_token, &command)
                .await;
            return;
        }
        if command.action.eq_ignore_ascii_case("list_sessions") {
            handle_local_list_sessions_command(state, tenant, user_id, context_token, &command)
                .await;
            return;
        }
        if command.action.eq_ignore_ascii_case(ACTIVATE_RECENT_ACTION) {
            match resolve_recent_session_activate_command(state, tenant, user_id, &command).await {
                Ok(resolved) => Some(resolved),
                Err(err) => {
                    warn!(
                        tenant_id = %tenant.tenant_id,
                        user_id,
                        action = %command.action,
                        error = %err,
                        "按序号切换微信会话失败"
                    );
                    let reply_text = build_local_command_error_message(&err);
                    if let Err(send_err) = wechat_api::send_text_to_user(
                        state,
                        tenant,
                        user_id,
                        &reply_text,
                        context_token,
                    )
                    .await
                    {
                        warn!(
                            tenant_id = %tenant.tenant_id,
                            user_id,
                            action = %command.action,
                            error = %send_err,
                            "微信本地切换会话失败提示发送失败"
                        );
                    }
                    return;
                }
            }
        } else {
            Some(command)
        }
    } else {
        None
    };

    if let Some(command) = inbound_command.as_ref() {
        if matches!(command.action.as_str(), "list_sessions" | "list_commands") {
            unreachable!("local command should have been handled before forwarding");
        }
    }

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
                "agentConfigId": command.agent_config_id,
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
            let gateway_ok = response_body
                .as_ref()
                .map(channel_gateway_response_is_success)
                .unwrap_or(true);
            if !gateway_ok {
                warn!(
                    tenant_id = %tenant.tenant_id,
                    user_id,
                    status = %status,
                    body = %response_text,
                    "channel-gateway 返回业务失败"
                );
                reply_lowcode_forward_failure(
                    state,
                    tenant,
                    user_id,
                    context_token,
                    Some(status.as_u16()),
                )
                .await;
                return;
            }
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

async fn handle_local_list_sessions_command(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    user_id: &str,
    context_token: Option<&str>,
    command: &InboundCommand,
) {
    let session_key = format!("wechat:dm:{user_id}");
    let reply_text = match channel_session_client::list_session_records(
        state,
        tenant,
        &session_key,
        SESSION_LIST_COMMAND_LIMIT,
    )
    .await
    {
        Ok(records) => build_session_list_text(&records),
        Err(err) => {
            warn!(
                tenant_id = %tenant.tenant_id,
                user_id,
                action = %command.action,
                error = %err,
                "查询微信渠道会话列表失败"
            );
            build_local_command_error_message(&err)
        }
    };

    if let Err(err) =
        wechat_api::send_text_to_user(state, tenant, user_id, &reply_text, context_token).await
    {
        warn!(
            tenant_id = %tenant.tenant_id,
            user_id,
            action = %command.action,
            error = %err,
            "微信本地命令回复发送失败"
        );
    }
}

async fn handle_local_list_commands_command(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    user_id: &str,
    context_token: Option<&str>,
    command: &InboundCommand,
) {
    let credential = tenant.credential.read().await;
    let assistant_name = credential
        .assistant_name
        .clone()
        .unwrap_or_else(|| state.config.runtime.assistant_name.clone());
    let command_actions = credential.command_actions.clone();
    drop(credential);

    let command_actions = match command_actions {
        Some(items) => items,
        None => state.effective_default_command_actions().await,
    };

    let reply_text = build_command_list_text(&assistant_name, &command_actions);
    if let Err(err) =
        wechat_api::send_text_to_user(state, tenant, user_id, &reply_text, context_token).await
    {
        warn!(
            tenant_id = %tenant.tenant_id,
            user_id,
            action = %command.action,
            error = %err,
            "微信本地命令回复发送失败"
        );
    }
}

async fn resolve_recent_session_activate_command(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    user_id: &str,
    command: &InboundCommand,
) -> Result<InboundCommand, crate::error::ServiceError> {
    let requested_index = command.recent_session_index.ok_or_else(|| {
        crate::error::ServiceError::BadRequest(
            "切换会话序号无效，请先发送“列会话”查看可用序号。".to_string(),
        )
    })?;
    let session_key = format!("wechat:dm:{user_id}");
    let records = channel_session_client::list_session_records(
        state,
        tenant,
        &session_key,
        SESSION_LIST_COMMAND_LIMIT,
    )
    .await?;
    let record = resolve_recent_session_record(&records, requested_index)?;

    Ok(InboundCommand {
        text: command.text.clone(),
        action: "activate".to_string(),
        agent_config_id: None,
        session_id: Some(record.session_id.clone()),
        recent_session_index: None,
        service: None,
        method: None,
        params: None,
    })
}

fn resolve_recent_session_record(
    records: &[ChannelSessionRecord],
    index: usize,
) -> Result<&ChannelSessionRecord, crate::error::ServiceError> {
    if records.is_empty() {
        return Err(crate::error::ServiceError::BadRequest(
            "当前还没有历史会话，请先发送“小百 列会话”确认最近会话。".to_string(),
        ));
    }
    if index == 0 || index > records.len() {
        return Err(crate::error::ServiceError::BadRequest(format!(
            "最近会话里只有 {} 个，请先发送“小百 列会话”查看可用序号。",
            records.len()
        )));
    }
    Ok(&records[index - 1])
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
    let reply_text = match status_code {
        Some(413) => LOWCODE_FORWARD_PAYLOAD_TOO_LARGE_MESSAGE,
        _ => LOWCODE_FORWARD_FALLBACK_MESSAGE,
    };

    if let Err(err) =
        wechat_api::send_text_to_user(state, tenant, user_id, reply_text, context_token).await
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

fn channel_gateway_response_is_success(payload: &Value) -> bool {
    payload
        .get("errorCode")
        .and_then(Value::as_str)
        .map(str::trim)
        .map(|code| code == "0")
        .unwrap_or(true)
}

fn build_session_list_text(records: &[ChannelSessionRecord]) -> String {
    if records.is_empty() {
        return "当前还没有历史会话。".to_string();
    }

    let mut lines = vec!["最近会话（新到旧）：".to_string()];
    for (index, record) in records.iter().enumerate() {
        let current = if record.is_current { " [当前]" } else { "" };
        lines.push(format!("{}. {}{}", index + 1, record.session_id, current));
        lines.push(format!(
            "最近活跃：{} | 消息数：{}",
            format_unix_timestamp(record.last_active_at_unix),
            record.message_count
        ));
        if let Some(preview) = record
            .latest_message_preview
            .as_deref()
            .and_then(normalize_preview_text)
        {
            lines.push(format!("预览：{}", truncate_preview(&preview, 60)));
        }
    }
    lines.push("发送“助手名 + 序号”可直接切换，例如：小百 2".to_string());
    lines.join("\n")
}

fn build_command_list_text(
    assistant_name: &str,
    command_actions: &[CommandActionConfig],
) -> String {
    let normalized_assistant_name =
        normalize_assistant_name(assistant_name).unwrap_or_else(|| "小百".to_string());
    let mut lines = vec!["当前可用命令：".to_string()];
    for item in command_actions {
        lines.push(format!(
            "- {} {}",
            normalized_assistant_name,
            item.text.trim()
        ));
    }
    if command_actions
        .iter()
        .any(|item| item.action.eq_ignore_ascii_case("list_sessions"))
    {
        lines.push(format!(
            "- {} <序号>：切换到最近会话列表里的对应会话",
            normalized_assistant_name
        ));
    }
    lines.join("\n")
}

fn build_local_command_error_message(err: &crate::error::ServiceError) -> String {
    match err {
        crate::error::ServiceError::BadRequest(message)
        | crate::error::ServiceError::NotFound(message)
        | crate::error::ServiceError::Unauthorized(message) => message.clone(),
        crate::error::ServiceError::Upstream(_) | crate::error::ServiceError::Internal(_) => {
            "暂时无法查询会话，请稍后重试。".to_string()
        }
    }
}

fn format_unix_timestamp(timestamp: i64) -> String {
    if timestamp <= 0 {
        return "-".to_string();
    }
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|datetime| datetime.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn truncate_preview(text: &str, limit: usize) -> String {
    let compact = text.replace('\n', " ");
    let compact = compact.trim();
    if compact.chars().count() <= limit {
        return compact.to_string();
    }
    compact.chars().take(limit).collect::<String>() + "..."
}

fn normalize_preview_text(text: &str) -> Option<String> {
    let normalized = text.replace('\r', "\n");
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
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
        ACTIVATE_RECENT_ACTION, DownloadedImageData, InboundCommand, SessionControlSuccess,
        build_command_list_text, build_image_data_url, build_lowcode_inbound_content,
        build_media_file_name, build_session_list_text, build_wechat_cdn_download_url,
        channel_gateway_response_is_success, decode_wechat_aes_key_hex, decrypt_aes_ecb,
        detect_inbound_command, effective_inline_image_max_bytes, guess_extension_from_mime,
        extract_session_control_success, extract_wechat_media_aes_key, guess_image_mime_type,
        has_media_items, looks_like_amr, looks_like_mp4, normalize_assistant_name,
        normalize_command_text, normalize_image_content_type, normalize_preview_text,
        parse_recent_session_index, replace_file_part_url, replace_image_part_url,
        resolve_media_mime_type, resolve_recent_session_record, session_control_success_reply_text,
    };
    use crate::channel_session_client::ChannelSessionRecord;
    use crate::config::CommandActionConfig;
    use aes::Aes128;
    use aes::cipher::{BlockEncryptMut, KeyInit, block_padding::Pkcs7};
    use ecb::Encryptor;
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
    fn caps_inline_image_bytes_for_transport_safety() {
        assert_eq!(effective_inline_image_max_bytes(0), 1);
        assert_eq!(effective_inline_image_max_bytes(32 * 1024), 32 * 1024);
        assert_eq!(effective_inline_image_max_bytes(5 * 1024 * 1024), 80 * 1024);
    }

    #[test]
    fn replaces_image_part_with_inline_data_url() {
        let part = json!({
            "type": "image_url",
            "image_url": {
                "url": "https://example.com/original.png"
            },
            "source": "wechat"
        });

        let updated = replace_image_part_url(part, "data:image/png;base64,AAAA");

        assert_eq!(
            updated["image_url"],
            json!({"url": "data:image/png;base64,AAAA"})
        );
        assert_eq!(updated["source"], "wechat");
    }

    #[test]
    fn preserves_remote_image_part_without_extra_metadata() {
        let part = json!({
            "type": "image_url",
            "image_url": {
                "url": "https://example.com/original.png"
            },
            "mediaType": "image",
            "source": "wechat",
            "wechatMedia": {
                "mid_size": 123
            }
        });

        let updated = replace_image_part_url(part, "https://example.com/original.png");

        assert_eq!(updated["type"], "image_url");
        assert_eq!(
            updated["image_url"],
            json!({"url": "https://example.com/original.png"})
        );
        assert_eq!(updated["mediaType"], "image");
        assert_eq!(updated["source"], "wechat");
    }

    #[test]
    fn replaces_file_part_url_without_changing_media_type() {
        let part = json!({
            "type": "file",
            "file": {
                "url": "https://example.com/original.bin"
            },
            "mediaType": "file",
            "source": "wechat"
        });

        let updated = replace_file_part_url(part, "https://example.com/uploaded.bin");

        assert_eq!(updated["type"], "file");
        assert_eq!(
            updated["file"],
            json!({"url": "https://example.com/uploaded.bin"})
        );
        assert_eq!(updated["mediaType"], "file");
        assert_eq!(updated["source"], "wechat");
    }

    #[test]
    fn builds_image_data_url_from_downloaded_bytes() {
        let image = DownloadedImageData {
            bytes: b"png-bytes".to_vec(),
            mime: "image/png",
        };
        assert_eq!(
            build_image_data_url(&image),
            "data:image/png;base64,cG5nLWJ5dGVz"
        );
    }

    #[test]
    fn builds_media_file_name_from_wechat_file_name_or_media_kind() {
        let part = json!({
            "wechatMedia": {
                "file_name": "report"
            }
        });
        assert_eq!(
            build_media_file_name(&part, "https://example.com/raw", "application/pdf", "file"),
            "report.pdf"
        );

        let part = json!({
            "wechatMedia": {}
        });
        assert_eq!(
            build_media_file_name(&part, "https://example.com/path/voice", "audio/amr", "audio"),
            "voice.amr"
        );
        assert_eq!(
            build_media_file_name(&part, "", "application/octet-stream", "file"),
            "wechat_file.bin"
        );
    }

    #[test]
    fn resolves_media_mime_type_for_audio_video_and_file() {
        assert_eq!(
            resolve_media_mime_type(Some("audio/mpeg"), b"", "audio"),
            "audio/mpeg"
        );
        assert_eq!(
            resolve_media_mime_type(None, b"#!AMR\npayload", "audio"),
            "audio/amr"
        );
        assert_eq!(
            resolve_media_mime_type(None, b"\x00\x00\x00\x18ftypisomrest", "video"),
            "video/mp4"
        );
        assert_eq!(
            resolve_media_mime_type(Some("application/octet-stream"), b"raw", "file"),
            "application/octet-stream"
        );
    }

    #[test]
    fn guesses_extensions_for_common_media_types() {
        assert_eq!(guess_extension_from_mime("audio/amr"), "amr");
        assert_eq!(guess_extension_from_mime("video/mp4"), "mp4");
        assert_eq!(guess_extension_from_mime("application/octet-stream"), "bin");
    }

    #[test]
    fn detects_amr_and_mp4_signatures() {
        assert!(looks_like_amr(b"#!AMR\nrest"));
        assert!(!looks_like_amr(b"not-amr"));
        assert!(looks_like_mp4(b"\x00\x00\x00\x18ftypisomrest"));
        assert!(!looks_like_mp4(b"short"));
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
                agent_config_id: None,
                session_id: None,
                service: None,
                method: None,
                params: None,
            },
            CommandActionConfig {
                text: "开始新话题".to_string(),
                action: "new".to_string(),
                agent_config_id: None,
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
                agent_config_id: None,
                session_id: None,
                recent_session_index: None,
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
                agent_config_id: None,
                session_id: None,
                recent_session_index: None,
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
                agent_config_id: None,
                session_id: None,
                service: None,
                method: None,
                params: None,
            },
            CommandActionConfig {
                text: "结束当前会话".to_string(),
                action: "abort".to_string(),
                agent_config_id: None,
                session_id: None,
                service: None,
                method: None,
                params: None,
            },
        ];
        assert_eq!(
            detect_inbound_command(
                Some(1),
                "小百 新话题 帮我总结一下",
                false,
                "小百",
                &commands
            ),
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
            agent_config_id: None,
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
                agent_config_id: None,
                session_id: None,
                recent_session_index: None,
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
            agent_config_id: None,
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
                agent_config_id: None,
                session_id: None,
                recent_session_index: None,
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
            agent_config_id: None,
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
                agent_config_id: None,
                session_id: Some("sess_order_123".to_string()),
                recent_session_index: None,
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
            agent_config_id: None,
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
                agent_config_id: None,
                session_id: None,
                recent_session_index: None,
                service: Some("crm-service".to_string()),
                method: Some("getPoints".to_string()),
                params: Some(json!({
                    "scene": "wechat"
                })),
            })
        );
    }

    #[test]
    fn detects_new_session_command_with_agent_config_id() {
        let commands = vec![CommandActionConfig {
            text: "切到客服助手".to_string(),
            action: "new".to_string(),
            agent_config_id: Some("agent_20001".to_string()),
            session_id: None,
            service: None,
            method: None,
            params: None,
        }];
        assert_eq!(
            detect_inbound_command(Some(1), "小百 切到客服助手", false, "小百", &commands),
            Some(InboundCommand {
                text: "切到客服助手".to_string(),
                action: "new".to_string(),
                agent_config_id: Some("agent_20001".to_string()),
                session_id: None,
                recent_session_index: None,
                service: None,
                method: None,
                params: None,
            })
        );
    }

    #[test]
    fn detects_list_sessions_command() {
        let commands = vec![CommandActionConfig {
            text: "列会话".to_string(),
            action: "list_sessions".to_string(),
            agent_config_id: None,
            session_id: None,
            service: None,
            method: None,
            params: None,
        }];
        assert_eq!(
            detect_inbound_command(Some(1), "小百 列会话", false, "小百", &commands),
            Some(InboundCommand {
                text: "列会话".to_string(),
                action: "list_sessions".to_string(),
                agent_config_id: None,
                session_id: None,
                recent_session_index: None,
                service: None,
                method: None,
                params: None,
            })
        );
    }

    #[test]
    fn detects_recent_session_index_command() {
        assert_eq!(
            detect_inbound_command(Some(1), "小百 2", false, "小百", &[]),
            Some(InboundCommand {
                text: "2".to_string(),
                action: ACTIVATE_RECENT_ACTION.to_string(),
                agent_config_id: None,
                session_id: None,
                recent_session_index: Some(2),
                service: None,
                method: None,
                params: None,
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
    fn parses_recent_session_index_from_numeric_text() {
        assert_eq!(parse_recent_session_index("1"), Some(1));
        assert_eq!(parse_recent_session_index(" 12 "), Some(12));
        assert_eq!(parse_recent_session_index("0"), None);
        assert_eq!(parse_recent_session_index("abc"), None);
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

    #[test]
    fn detects_channel_gateway_business_success_by_error_code() {
        assert!(channel_gateway_response_is_success(&json!({
            "errorCode": "0",
            "value": "success"
        })));
        assert!(!channel_gateway_response_is_success(&json!({
            "errorCode": "UPSTREAM_ERROR",
            "value": "调用失败"
        })));
    }

    #[test]
    fn builds_session_list_text_with_current_marker() {
        let text = build_session_list_text(&[
            ChannelSessionRecord {
                session_id: "sess_current".to_string(),
                created_at_unix: 1_700_000_000,
                last_active_at_unix: 1_700_000_100,
                is_current: true,
                message_count: 12,
                latest_message_preview: Some("你好\n继续".to_string()),
            },
            ChannelSessionRecord {
                session_id: "sess_old".to_string(),
                created_at_unix: 1_699_999_000,
                last_active_at_unix: 1_699_999_100,
                is_current: false,
                message_count: 3,
                latest_message_preview: None,
            },
        ]);

        assert!(text.contains("1. sess_current [当前]"));
        assert!(text.contains("2. sess_old"));
        assert!(text.contains("预览：你好 继续"));
        assert!(text.contains("小百 2"));
    }

    #[test]
    fn builds_command_list_text_with_recent_session_hint() {
        let text = build_command_list_text(
            " 小百 ",
            &[
                CommandActionConfig {
                    text: "新话题".to_string(),
                    action: "new".to_string(),
                    agent_config_id: None,
                    session_id: None,
                    service: None,
                    method: None,
                    params: None,
                },
                CommandActionConfig {
                    text: "命令列表".to_string(),
                    action: "list_commands".to_string(),
                    agent_config_id: None,
                    session_id: None,
                    service: None,
                    method: None,
                    params: None,
                },
                CommandActionConfig {
                    text: "列会话".to_string(),
                    action: "list_sessions".to_string(),
                    agent_config_id: None,
                    session_id: None,
                    service: None,
                    method: None,
                    params: None,
                },
            ],
        );

        assert!(text.contains("当前可用命令："));
        assert!(text.contains("- 小百 新话题"));
        assert!(text.contains("- 小百 命令列表"));
        assert!(text.contains("- 小百 列会话"));
        assert!(text.contains("- 小百 <序号>：切换到最近会话列表里的对应会话"));
    }

    #[test]
    fn resolves_recent_session_record_by_one_based_index() {
        let records = vec![
            ChannelSessionRecord {
                session_id: "sess_current".to_string(),
                created_at_unix: 1_700_000_000,
                last_active_at_unix: 1_700_000_100,
                is_current: true,
                message_count: 12,
                latest_message_preview: Some("你好".to_string()),
            },
            ChannelSessionRecord {
                session_id: "sess_old".to_string(),
                created_at_unix: 1_699_999_000,
                last_active_at_unix: 1_699_999_100,
                is_current: false,
                message_count: 3,
                latest_message_preview: None,
            },
        ];

        assert_eq!(
            resolve_recent_session_record(&records, 2)
                .expect("should resolve by one-based index")
                .session_id,
            "sess_old"
        );
        assert!(resolve_recent_session_record(&records, 0).is_err());
        assert!(resolve_recent_session_record(&records, 3).is_err());
    }

    #[test]
    fn normalizes_preview_text() {
        assert_eq!(
            normalize_preview_text(" \nhello\r\n "),
            Some("hello".to_string())
        );
        assert_eq!(normalize_preview_text("   "), None);
    }
}
