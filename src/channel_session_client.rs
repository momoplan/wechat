use crate::error::ServiceError;
use crate::state::{AppState, TenantContext};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

const LIST_SESSION_RECORDS_PATH: &str = "sessions/list";

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelSessionRecord {
    pub session_id: String,
    pub created_at_unix: i64,
    pub last_active_at_unix: i64,
    pub is_current: bool,
    pub message_count: u64,
    pub latest_message_preview: Option<String>,
}

pub async fn list_session_records(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    session_key: &str,
    limit: usize,
) -> Result<Vec<ChannelSessionRecord>, ServiceError> {
    let (endpoint, gateway_token) = resolve_session_records_context(state, tenant).await?;
    let mut request = state.http_client.post(endpoint).json(&json!({
        "sessionKey": session_key,
        "limit": limit.max(1),
    }));
    if let Some(token) = gateway_token {
        request = request.bearer_auth(token);
    }

    let response = request.send().await.map_err(|err| {
        ServiceError::Upstream(format!("查询 channel-gateway 会话列表失败: {err}"))
    })?;
    let status = response.status();
    let raw_body = response.text().await.map_err(|err| {
        ServiceError::Upstream(format!("读取 channel-gateway 会话列表响应失败: {err}"))
    })?;
    if !status.is_success() {
        return Err(map_gateway_error(
            status.as_u16(),
            &raw_body,
            "查询 channel-gateway 会话列表失败",
        ));
    }

    serde_json::from_str::<Vec<ChannelSessionRecord>>(&raw_body).map_err(|err| {
        ServiceError::Upstream(format!(
            "解析 channel-gateway 会话列表响应失败: {err}; body={raw_body}"
        ))
    })
}

async fn resolve_session_records_context(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
) -> Result<(String, Option<String>), ServiceError> {
    let credential = tenant.credential.read().await.clone();
    let gateway_url = credential
        .lowcode_ws_base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ServiceError::BadRequest("租户未配置 gateway_url".to_string()))?;
    let endpoint = build_list_session_records_endpoint(gateway_url)?;
    let gateway_token = credential.lowcode_ws_token.clone().or_else(|| {
        state
            .config
            .channel_gateway
            .inbound_token
            .clone()
            .filter(|value| !value.trim().is_empty())
    });
    Ok((endpoint, gateway_token))
}

pub fn build_list_session_records_endpoint(gateway_url: &str) -> Result<String, ServiceError> {
    let trimmed = gateway_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(ServiceError::BadRequest(
            "租户未配置 gateway_url".to_string(),
        ));
    }

    let route_base = if trimmed.ends_with("/inbound") {
        trimmed.trim_end_matches("/inbound").trim_end_matches('/')
    } else {
        trimmed
    };

    if route_base.is_empty() {
        return Err(ServiceError::BadRequest(
            "租户未配置 gateway_url".to_string(),
        ));
    }

    Ok(format!("{route_base}/{LIST_SESSION_RECORDS_PATH}"))
}

fn map_gateway_error(status: u16, raw_body: &str, prefix: &str) -> ServiceError {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw_body) {
        if let Some(message) = extract_gateway_error_message(&value) {
            return match status {
                400 => ServiceError::BadRequest(message),
                401 | 403 => ServiceError::Unauthorized(message),
                404 => ServiceError::NotFound(message),
                _ => ServiceError::Upstream(format!("{prefix}: {message}")),
            };
        }
    }
    ServiceError::Upstream(format!("{prefix}: status={status} body={raw_body}"))
}

fn extract_gateway_error_message(value: &serde_json::Value) -> Option<String> {
    [
        "/message",
        "/error",
        "/value",
        "/data/message",
        "/data/error",
    ]
    .iter()
    .find_map(|pointer| {
        value
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(ToOwned::to_owned)
    })
}

#[cfg(test)]
mod tests {
    use super::build_list_session_records_endpoint;

    #[test]
    fn builds_session_list_endpoint_from_standard_channel_route() {
        assert_eq!(
            build_list_session_records_endpoint(
                "http://127.0.0.1:4020/workspaces/acme/channels/wechat"
            )
            .unwrap(),
            "http://127.0.0.1:4020/workspaces/acme/channels/wechat/sessions/list"
        );
    }

    #[test]
    fn builds_session_list_endpoint_from_inbound_route() {
        assert_eq!(
            build_list_session_records_endpoint(
                "http://127.0.0.1:4020/workspaces/acme/channels/wechat/inbound"
            )
            .unwrap(),
            "http://127.0.0.1:4020/workspaces/acme/channels/wechat/sessions/list"
        );
    }

    #[test]
    fn builds_session_list_endpoint_from_service_route() {
        assert_eq!(
            build_list_session_records_endpoint("https://gateway.example.com/svc-channel-gateway")
                .unwrap(),
            "https://gateway.example.com/svc-channel-gateway/sessions/list"
        );
    }
}
