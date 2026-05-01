use crate::error::ServiceError;
use crate::state::{AppState, TenantContext};
use reqwest::header::{CONTENT_TYPE, HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::sync::Arc;

const PREPARE_FILE_UPLOAD_METHOD: &str = "prepareFileUpload";
const COMPLETE_FILE_UPLOAD_METHOD: &str = "completeFileUpload";

#[derive(Debug, Clone)]
pub struct ProjectFileUploadResult {
    pub download_url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreparedFileUpload {
    upload_key: String,
    upload_url: String,
    method: String,
    #[serde(default)]
    headers: Map<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompletedFileUpload {
    download_url: String,
}

pub async fn upload_bytes(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    workspace_id: &str,
    channel_user_id: &str,
    file_name: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<ProjectFileUploadResult, ServiceError> {
    let route_base = resolve_gateway_route_base(tenant).await?;
    let gateway_token = resolve_gateway_token(state, tenant).await;
    let size = u64::try_from(bytes.len())
        .map_err(|_| ServiceError::BadRequest("项目文件过大".to_string()))?;
    let prepared = prepare_file_upload(
        state,
        &route_base,
        gateway_token.as_deref(),
        workspace_id,
        channel_user_id,
        file_name,
        content_type,
        size,
    )
    .await?;

    upload_bytes_to_presigned_url(
        &state.http_client,
        &prepared.upload_url,
        &prepared.method,
        &prepared.headers,
        bytes,
        content_type,
    )
    .await?;

    let completed = complete_file_upload(
        state,
        &route_base,
        gateway_token.as_deref(),
        workspace_id,
        channel_user_id,
        &prepared.upload_key,
    )
    .await?;

    Ok(ProjectFileUploadResult {
        download_url: completed.download_url,
    })
}

pub async fn upload_image_bytes(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    workspace_id: &str,
    channel_user_id: &str,
    file_name: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<ProjectFileUploadResult, ServiceError> {
    upload_bytes(
        state,
        tenant,
        workspace_id,
        channel_user_id,
        file_name,
        content_type,
        bytes,
    )
    .await
}

async fn prepare_file_upload(
    state: &AppState,
    route_base: &str,
    gateway_token: Option<&str>,
    workspace_id: &str,
    channel_user_id: &str,
    file_name: &str,
    content_type: &str,
    size: u64,
) -> Result<PreparedFileUpload, ServiceError> {
    let endpoint = format!("{route_base}/{PREPARE_FILE_UPLOAD_METHOD}");
    let payload = json!({
        "workspaceId": workspace_id,
        "channel": "wechat",
        "channelUserId": channel_user_id,
        "fileName": file_name,
        "contentType": content_type,
        "size": size,
    });
    request_gateway_json(state, &endpoint, gateway_token, &payload, "申请项目文件上传凭证").await
}

async fn complete_file_upload(
    state: &AppState,
    route_base: &str,
    gateway_token: Option<&str>,
    workspace_id: &str,
    channel_user_id: &str,
    upload_key: &str,
) -> Result<CompletedFileUpload, ServiceError> {
    let endpoint = format!("{route_base}/{COMPLETE_FILE_UPLOAD_METHOD}");
    let payload = json!({
        "workspaceId": workspace_id,
        "channel": "wechat",
        "channelUserId": channel_user_id,
        "uploadKey": upload_key,
    });
    request_gateway_json(state, &endpoint, gateway_token, &payload, "完成项目文件上传").await
}

async fn request_gateway_json<T: for<'de> Deserialize<'de>>(
    state: &AppState,
    endpoint: &str,
    gateway_token: Option<&str>,
    payload: &Value,
    action: &str,
) -> Result<T, ServiceError> {
    let mut request = state.http_client.post(endpoint).json(payload);
    if let Some(token) = gateway_token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .map_err(|err| ServiceError::Upstream(format!("{action}失败: {err}")))?;
    let status = response.status();
    let raw_body = response
        .text()
        .await
        .map_err(|err| ServiceError::Upstream(format!("读取 channel-gateway 响应失败: {err}")))?;
    if !status.is_success() {
        return Err(map_gateway_error(status.as_u16(), &raw_body, action));
    }
    serde_json::from_str::<T>(&raw_body).map_err(|err| {
        ServiceError::Upstream(format!(
            "解析 channel-gateway 响应失败: {err}; body={raw_body}"
        ))
    })
}

async fn resolve_gateway_route_base(
    tenant: &Arc<TenantContext>,
) -> Result<String, ServiceError> {
    let credential = tenant.credential.read().await.clone();
    let gateway_url = credential
        .lowcode_ws_base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ServiceError::BadRequest("租户未配置 gateway_url".to_string()))?;
    let trimmed = gateway_url.trim().trim_end_matches('/');
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
    Ok(route_base.to_string())
}

async fn resolve_gateway_token(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
) -> Option<String> {
    let credential = tenant.credential.read().await.clone();
    credential.lowcode_ws_token.clone().or_else(|| {
        state
            .config
            .channel_gateway
            .inbound_token
            .clone()
            .filter(|value| !value.trim().is_empty())
    })
}

async fn upload_bytes_to_presigned_url(
    client: &reqwest::Client,
    upload_url: &str,
    method: &str,
    headers: &Map<String, Value>,
    bytes: &[u8],
    content_type: &str,
) -> Result<(), ServiceError> {
    if !method.trim().eq_ignore_ascii_case("PUT") {
        return Err(ServiceError::Upstream(format!(
            "unsupported project upload method: {method}"
        )));
    }
    let mut request = client.put(upload_url);
    let mut has_content_type_header = false;
    for (name, value) in headers {
        let Some(raw) = value.as_str() else {
            continue;
        };
        let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|err| {
            ServiceError::Upstream(format!("invalid upload header name {name}: {err}"))
        })?;
        if header_name == CONTENT_TYPE {
            has_content_type_header = true;
        }
        let header_value = HeaderValue::from_str(raw).map_err(|err| {
            ServiceError::Upstream(format!("invalid upload header value for {name}: {err}"))
        })?;
        request = request.header(header_name, header_value);
    }
    if !has_content_type_header {
        request = request.header(CONTENT_TYPE, content_type);
    }
    let response = request
        .body(bytes.to_vec())
        .send()
        .await
        .map_err(|err| ServiceError::Upstream(format!("上传项目文件失败: {err}")))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(ServiceError::Upstream(format!(
            "上传项目文件失败 HTTP {}: {}",
            status.as_u16(),
            body
        )));
    }
    Ok(())
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
