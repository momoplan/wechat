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

    if message_type != Some(1) {
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

    maybe_forward_to_lowcode_agent(
        state,
        tenant,
        &msg,
        &from_user_id,
        &content,
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

async fn maybe_forward_to_lowcode_agent(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    raw_message: &Value,
    user_id: &str,
    content: &str,
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
    let workspace_id = credential
        .workspace_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
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
            "workspaceId": workspace_id,
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
