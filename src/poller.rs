use crate::models::ReceivedEvent;
use crate::state::{AppState, TenantContext};
use crate::wechat_api;
use chrono::Utc;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{error, info, warn};

const LOWCODE_FORWARD_FALLBACK_MESSAGE: &str = "消息暂时处理失败，请稍后重试或联系管理员。";
const WECHAT_CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";

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

    maybe_forward_to_lowcode_agent(
        state,
        tenant,
        &msg,
        &from_user_id,
        inbound_content,
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
                parts.push(build_media_content_part(
                    item,
                    "input_image",
                    "image_url",
                    "image",
                ));
                has_media = true;
            }
            3 => {
                parts.push(build_media_content_part(
                    item,
                    "input_file",
                    "file_url",
                    "audio",
                ));
                has_media = true;
            }
            4 => {
                parts.push(build_media_content_part(
                    item,
                    "input_file",
                    "file_url",
                    "file",
                ));
                has_media = true;
            }
            5 => {
                parts.push(build_media_content_part(
                    item,
                    "input_video",
                    "video_url",
                    "video",
                ));
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
    url_key: &str,
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
        object.insert(
            url_key.to_string(),
            download_url.map(Value::String).unwrap_or(Value::Null),
        );
    }

    part
}

fn build_wechat_cdn_download_url(encrypted_query_param: &str) -> String {
    format!(
        "{WECHAT_CDN_BASE_URL}/download?encrypted_query_param={}",
        urlencoding::encode(encrypted_query_param)
    )
}

async fn maybe_forward_to_lowcode_agent(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    raw_message: &Value,
    user_id: &str,
    content: Value,
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
            "userId": user_id,
            "contextToken": context_token
        },
        "raw": raw_message,
        "data": {
            "type": "input",
            "source": "wechat",
            "tenantId": tenant.tenant_id.as_str(),
            "messageType": raw_message.get("message_type"),
            "fromUserId": user_id,
            "contextToken": context_token,
            "content": content
        }
    });

    let mut request = state.http_client.post(endpoint).json(&body);
    if let Some(token) = auth_token {
        request = request.bearer_auth(token);
    }

    match request.send().await {
        Ok(response) if response.status().is_success() => {
            info!(tenant_id = %tenant.tenant_id, user_id, "微信消息已转发到 channel-gateway");
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

#[cfg(test)]
mod tests {
    use super::{build_lowcode_inbound_content, build_wechat_cdn_download_url, has_media_items};
    use serde_json::json;

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
        assert_eq!(content[1]["type"], "input_image");
        assert_eq!(
            content[1]["image_url"],
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
        assert_eq!(content[1]["type"], "input_video");
        assert_eq!(
            content[1]["video_url"],
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
        assert_eq!(content[1]["type"], "input_file");
        assert_eq!(content[1]["mediaType"], "audio");
        assert_eq!(
            content[1]["file_url"],
            build_wechat_cdn_download_url("voice-token")
        );
        assert_eq!(content[1]["wechatMedia"]["playtime"], 3760);
    }
}
