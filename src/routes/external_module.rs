use crate::models::TenantCredential;
use crate::routes::management::{merge_properties, normalize_credential, normalize_optional};
use crate::state::{AppState, TenantContext};
use actix_web::{HttpResponse, Scope, web};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ExternalModuleCreateRequest {
    #[serde(default)]
    app_instance_id: Option<String>,
    #[serde(default)]
    user_id: Option<i64>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    tenant_id: Option<String>,
    #[serde(default)]
    properties: Option<Map<String, Value>>,
    #[serde(default)]
    current_properties: Option<Map<String, Value>>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ExternalModuleUpdatePropertiesRequest {
    #[serde(default)]
    tenant_id: Option<String>,
    #[serde(default)]
    properties: Option<Map<String, Value>>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ModuleQuery {
    #[serde(default)]
    module_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExternalModuleResponseData {
    instance_id: String,
    id: String,
    external_id: String,
    status: String,
    properties: Map<String, Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExternalModuleRuntimeStatusData {
    provider: String,
    instance_id: String,
    external_id: String,
    configured: bool,
    running: bool,
    connected: bool,
    state: String,
    connected_at_unix: Option<i64>,
    last_heartbeat_at_unix: Option<i64>,
    last_event_at_unix: Option<i64>,
    last_error: Option<String>,
    last_error_at_unix: Option<i64>,
    last_disconnect_at_unix: Option<i64>,
    last_disconnect_reason: Option<String>,
    reconnect_count: u64,
    observed_at_unix: i64,
}

pub fn scope() -> Scope {
    web::scope("/external-module")
        .route("", web::post().to(create_external_module))
        .route("/updateProperties", web::post().to(update_properties))
        .route("/{external_id}", web::get().to(get_external_module))
        .route(
            "/{external_id}/runtimeStatus",
            web::get().to(get_runtime_status),
        )
        .route("/{external_id}", web::delete().to(delete_external_module))
        .route("/{external_id}/stop", web::post().to(stop_external_module))
}

async fn create_external_module(
    state: web::Data<Arc<AppState>>,
    payload: web::Json<ExternalModuleCreateRequest>,
) -> HttpResponse {
    let request = payload.into_inner();
    let mut properties = merge_properties(
        request.properties.clone(),
        request.current_properties.clone(),
    );
    let external_id = match resolve_external_id(&request, &properties) {
        Ok(value) => value,
        Err(err) => return fail_response(err),
    };
    let existing = read_existing_credential(state.get_ref(), &external_id).await;
    let mut next = match build_credential(&properties, existing.as_ref(), true) {
        Ok(value) => value,
        Err(err) => return fail_response(err),
    };
    if let Err(err) = normalize_credential(&mut next) {
        return fail_response(err.to_string());
    }

    let (tenant, _) = match state.upsert_tenant(&external_id, next).await {
        Ok(result) => result,
        Err(err) => return fail_response(format!("创建租户失败: {err}")),
    };
    maybe_restart_worker(state.get_ref(), tenant.clone()).await;

    let summary = tenant.summary().await;
    let credential = tenant.credential.read().await.clone();
    properties = merge_response_properties(properties, &external_id, &summary, &credential);
    if let Some(user_id) = request.user_id {
        properties.insert("userId".to_string(), Value::Number(user_id.into()));
    }
    if let Some(version) = normalize_optional(request.version) {
        properties.insert("version".to_string(), Value::String(version));
    }
    if let Some(app_instance_id) = normalize_optional(request.app_instance_id) {
        properties.insert(
            "applicationRuntimeId".to_string(),
            Value::String(app_instance_id),
        );
    }

    success_response(ExternalModuleResponseData {
        instance_id: external_id.clone(),
        id: external_id.clone(),
        external_id,
        status: module_status(&summary),
        properties,
    })
}

async fn get_external_module(
    path: web::Path<String>,
    state: web::Data<Arc<AppState>>,
    query: web::Query<ModuleQuery>,
) -> HttpResponse {
    let external_id = path.into_inner();
    let Some(tenant) = state.get_tenant(&external_id) else {
        return fail_response(format!("外部实例不存在: {external_id}"));
    };

    let summary = tenant.summary().await;
    let credential = tenant.credential.read().await.clone();
    let mut properties = merge_response_properties(Map::new(), &external_id, &summary, &credential);
    if let Some(module_id) = normalize_optional(query.module_id.clone()) {
        properties.insert("moduleId".to_string(), Value::String(module_id));
    }

    success_response(ExternalModuleResponseData {
        instance_id: external_id.clone(),
        id: external_id.clone(),
        external_id,
        status: module_status(&summary),
        properties,
    })
}

async fn get_runtime_status(
    path: web::Path<String>,
    state: web::Data<Arc<AppState>>,
) -> HttpResponse {
    let external_id = path.into_inner();
    let Some(tenant) = state.get_tenant(&external_id) else {
        return fail_response(format!("外部实例不存在: {external_id}"));
    };

    let summary = tenant.summary().await;
    let connection = summary.connection;

    success_json_response(build_runtime_status(
        "wechat",
        &external_id,
        summary.logged_in,
        connection,
    ))
}

async fn delete_external_module(
    path: web::Path<String>,
    state: web::Data<Arc<AppState>>,
) -> HttpResponse {
    let external_id = path.into_inner();
    if let Some(tenant) = state.get_tenant(&external_id) {
        let _ = state.stop_tenant_worker(&tenant).await;
    }
    if let Err(err) = state.remove_tenant(&external_id).await {
        return fail_response(format!("删除租户失败: {err}"));
    }

    success_response(ExternalModuleResponseData {
        instance_id: external_id.clone(),
        id: external_id.clone(),
        external_id: external_id.clone(),
        status: "STOPPED".to_string(),
        properties: {
            let mut map = Map::new();
            map.insert("tenantId".to_string(), data_string_value(external_id));
            map
        },
    })
}

async fn stop_external_module(
    path: web::Path<String>,
    state: web::Data<Arc<AppState>>,
) -> HttpResponse {
    let external_id = path.into_inner();
    let tenant = match state.set_tenant_enabled(&external_id, false).await {
        Ok(tenant) => tenant,
        Err(err) => return fail_response(err.to_string()),
    };
    let _ = state.stop_tenant_worker(&tenant).await;
    let summary = tenant.summary().await;
    let mut properties = Map::new();
    properties.insert(
        "tenantId".to_string(),
        data_string_value(external_id.clone()),
    );

    success_response(ExternalModuleResponseData {
        instance_id: external_id.clone(),
        id: external_id.clone(),
        external_id,
        status: module_status(&summary),
        properties,
    })
}

async fn update_properties(
    state: web::Data<Arc<AppState>>,
    payload: web::Json<ExternalModuleUpdatePropertiesRequest>,
    _query: web::Query<ModuleQuery>,
) -> HttpResponse {
    let request = payload.into_inner();
    let properties = request.properties.clone().unwrap_or_default();
    let external_id = match resolve_target_external_id(&request, &properties) {
        Ok(value) => value,
        Err(err) => return fail_response(err),
    };
    let Some(external_id) = external_id else {
        return success_response(ExternalModuleResponseData {
            instance_id: "".to_string(),
            id: "".to_string(),
            external_id: "".to_string(),
            status: "IGNORED".to_string(),
            properties,
        });
    };

    let existing = match read_existing_credential(state.get_ref(), &external_id).await {
        Some(value) => value,
        None => return fail_response(format!("外部实例不存在: {external_id}")),
    };
    let mut next = match build_credential(&properties, Some(&existing), existing.is_enabled()) {
        Ok(value) => value,
        Err(err) => return fail_response(err),
    };
    if let Err(err) = normalize_credential(&mut next) {
        return fail_response(err.to_string());
    }

    if let Some(auto_start) = match extract_data_bool(&properties, &["autoStart", "auto_start"]) {
        Ok(value) => value,
        Err(err) => return fail_response(err),
    } {
        next.enabled = Some(auto_start);
    }

    let (tenant, _) = match state.upsert_tenant(&external_id, next).await {
        Ok(result) => result,
        Err(err) => return fail_response(format!("更新租户失败: {err}")),
    };
    maybe_restart_worker(state.get_ref(), tenant.clone()).await;

    let summary = tenant.summary().await;
    let credential = tenant.credential.read().await.clone();
    let response_properties =
        merge_response_properties(properties, &external_id, &summary, &credential);
    success_response(ExternalModuleResponseData {
        instance_id: external_id.clone(),
        id: external_id.clone(),
        external_id,
        status: module_status(&summary),
        properties: response_properties,
    })
}

async fn maybe_restart_worker(state: &Arc<AppState>, tenant: Arc<TenantContext>) {
    let credential = tenant.credential.read().await.clone();
    let _ = state.stop_tenant_worker(&tenant).await;
    if credential.is_enabled() && credential.is_logged_in() {
        let _ = state.start_tenant_worker(tenant).await;
    }
}

async fn read_existing_credential(
    state: &Arc<AppState>,
    external_id: &str,
) -> Option<TenantCredential> {
    let tenant = state.get_tenant(external_id)?;
    Some(tenant.credential.read().await.clone())
}

fn resolve_external_id(
    request: &ExternalModuleCreateRequest,
    properties: &Map<String, Value>,
) -> Result<String, String> {
    if let Some(external_id) = normalize_optional(request.tenant_id.clone()) {
        return Ok(external_id);
    }

    Ok(extract_data_string(properties, &["tenantId", "tenant_id"])?
        .unwrap_or_else(|| Uuid::new_v4().to_string()))
}

fn resolve_target_external_id(
    request: &ExternalModuleUpdatePropertiesRequest,
    properties: &Map<String, Value>,
) -> Result<Option<String>, String> {
    if let Some(external_id) = normalize_optional(request.tenant_id.clone()) {
        return Ok(Some(external_id));
    }

    Ok(
        extract_data_string(properties, &["tenantId", "tenant_id"])?.or(extract_data_string(
            properties,
            &["externalId", "external_id"],
        )?),
    )
}

fn build_credential(
    properties: &Map<String, Value>,
    existing: Option<&TenantCredential>,
    enabled_default: bool,
) -> Result<TenantCredential, String> {
    let gateway_reference =
        extract_external_service_reference(properties, &["gatewayUrl", "gateway_url"])?;

    Ok(TenantCredential {
        bot_token: extract_data_string(properties, &["botToken", "bot_token", "token"])?
            .or_else(|| existing.and_then(|value| value.bot_token.clone())),
        api_base_url: extract_data_string(properties, &["baseUrl", "apiBaseUrl", "api_base_url"])?
            .or_else(|| existing.and_then(|value| value.api_base_url.clone())),
        account_id: extract_data_string(properties, &["accountId", "account_id"])?
            .or_else(|| existing.and_then(|value| value.account_id.clone())),
        user_id: extract_data_string(properties, &["userId", "user_id"])?
            .or_else(|| existing.and_then(|value| value.user_id.clone())),
        sync_buf: existing.and_then(|value| value.sync_buf.clone()),
        lowcode_ws_base_url: gateway_reference
            .as_ref()
            .and_then(|value| value.url.clone())
            .or_else(|| existing.and_then(|value| value.lowcode_ws_base_url.clone())),
        lowcode_ws_token: extract_data_string(properties, &["gatewayToken", "gateway_token"])?
            .or_else(|| {
                gateway_reference
                    .as_ref()
                    .and_then(|value| value.token.clone())
            })
            .or_else(|| existing.and_then(|value| value.lowcode_ws_token.clone())),
        outbound_token: extract_data_string(
            properties,
            &[
                "outboundToken",
                "outbound_token",
                "outboundCallbackToken",
                "callbackToken",
                "channelCallbackToken",
            ],
        )?
        .or_else(|| existing.and_then(|value| value.outbound_token.clone()))
        .or_else(|| Some(crate::models::generate_outbound_token())),
        lowcode_forward_enabled: extract_data_bool(
            properties,
            &["lowcodeForwardEnabled", "lowcode_forward_enabled"],
        )?
        .or_else(|| existing.and_then(|value| value.lowcode_forward_enabled)),
        enabled: Some(
            extract_data_bool(properties, &["autoStart", "auto_start"])?
                .or_else(|| existing.map(|value| value.is_enabled()))
                .unwrap_or(enabled_default),
        ),
        command_actions: existing.and_then(|value| value.command_actions.clone()),
    })
}

fn extract_data_string(
    properties: &Map<String, Value>,
    keys: &[&str],
) -> Result<Option<String>, String> {
    let Some((key, value)) = find_property(properties, keys) else {
        return Ok(None);
    };
    let raw = extract_typed_value(value, key, "Data")?;
    match raw {
        Value::Null => Ok(None),
        Value::String(text) => Ok(normalize_optional(Some(text.clone()))),
        _ => Err(format!("属性 {key} 的 Data.value 必须为字符串")),
    }
}

fn extract_data_bool(
    properties: &Map<String, Value>,
    keys: &[&str],
) -> Result<Option<bool>, String> {
    let Some((key, value)) = find_property(properties, keys) else {
        return Ok(None);
    };
    let raw = extract_typed_value(value, key, "Data")?;
    match raw {
        Value::Null => Ok(None),
        Value::Bool(flag) => Ok(Some(*flag)),
        _ => Err(format!("属性 {key} 的 Data.value 必须为布尔值")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExternalServiceReferencePayload {
    url: Option<String>,
    token: Option<String>,
}

fn extract_external_service_reference(
    properties: &Map<String, Value>,
    keys: &[&str],
) -> Result<Option<ExternalServiceReferencePayload>, String> {
    let Some((key, value)) = find_property(properties, keys) else {
        return Ok(None);
    };
    let object = value
        .as_object()
        .ok_or_else(|| format!("属性 {key} 必须为 ExternalServiceReference 类型"))?;
    let type_name = object
        .get("@type")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("属性 {key} 必须为 ExternalServiceReference 类型"))?;
    if type_name != "ExternalServiceReference" {
        return Err(format!("属性 {key} 必须为 ExternalServiceReference 类型"));
    }
    let url = object
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("属性 {key} 的 ExternalServiceReference.url 必须为字符串"))?;
    let token = match object.get("token") {
        Some(Value::Null) | None => None,
        Some(Value::String(text)) => normalize_optional(Some(text.clone())),
        Some(_) => {
            return Err(format!(
                "属性 {key} 的 ExternalServiceReference.token 必须为字符串"
            ));
        }
    };
    Ok(Some(ExternalServiceReferencePayload {
        url: normalize_optional(Some(url.to_string())),
        token,
    }))
}

fn find_property<'a>(
    properties: &'a Map<String, Value>,
    keys: &[&'a str],
) -> Option<(&'a str, &'a Value)> {
    keys.iter()
        .find_map(|key| properties.get(*key).map(|value| (*key, value)))
}

fn extract_typed_value<'a>(
    value: &'a Value,
    key: &str,
    expected_type: &str,
) -> Result<&'a Value, String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("属性 {key} 必须为 {expected_type} 类型"))?;
    let type_name = object
        .get("@type")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("属性 {key} 必须为 {expected_type} 类型"))?;
    if type_name != expected_type {
        return Err(format!("属性 {key} 必须为 {expected_type} 类型"));
    }
    object
        .get("value")
        .ok_or_else(|| format!("属性 {key} 缺少 {expected_type}.value"))
}

fn data_string_value(value: impl Into<String>) -> Value {
    json!({
        "@type": "Data",
        "value": value.into()
    })
}

fn data_bool_value(value: bool) -> Value {
    json!({
        "@type": "Data",
        "value": value
    })
}

fn external_service_reference_value(url: impl Into<String>) -> Value {
    json!({
        "@type": "ExternalServiceReference",
        "url": url.into()
    })
}

fn merge_response_properties(
    mut properties: Map<String, Value>,
    external_id: &str,
    summary: &crate::models::TenantSummary,
    credential: &TenantCredential,
) -> Map<String, Value> {
    properties.insert(
        "tenantId".to_string(),
        data_string_value(external_id.to_string()),
    );
    properties.insert(
        "instanceId".to_string(),
        data_string_value(external_id.to_string()),
    );
    properties.insert(
        "externalId".to_string(),
        data_string_value(external_id.to_string()),
    );
    properties.insert("loggedIn".to_string(), Value::Bool(summary.logged_in));
    if let Some(base_url) = summary.api_base_url.clone() {
        properties.insert("baseUrl".to_string(), data_string_value(base_url));
    }
    if let Some(account_id) = summary.account_id.clone() {
        properties.insert("accountId".to_string(), data_string_value(account_id));
    }
    if let Some(user_id) = summary.user_id.clone() {
        properties.insert("userId".to_string(), data_string_value(user_id));
    }
    if let Some(url) = summary.lowcode_ws_base_url.clone() {
        properties.insert(
            "gatewayUrl".to_string(),
            external_service_reference_value(url),
        );
    }
    if let Some(token) = credential.outbound_token.clone() {
        properties.insert("outboundToken".to_string(), data_string_value(token));
    }
    if let Some(enabled) = summary.lowcode_forward_enabled {
        properties.insert(
            "lowcodeForwardEnabled".to_string(),
            data_bool_value(enabled),
        );
    }
    properties
}

fn module_status(summary: &crate::models::TenantSummary) -> String {
    if !summary.enabled {
        "STOPPED".to_string()
    } else if summary.connection.running {
        "RUNNING".to_string()
    } else if summary.logged_in {
        "CREATED".to_string()
    } else {
        "PENDING".to_string()
    }
}

fn build_runtime_status(
    provider: &str,
    external_id: &str,
    configured: bool,
    connection: crate::models::ConnectionStatus,
) -> ExternalModuleRuntimeStatusData {
    let state = if !connection.running {
        "stopped"
    } else if connection.connected {
        "connected"
    } else {
        "pending"
    };

    ExternalModuleRuntimeStatusData {
        provider: provider.to_string(),
        instance_id: external_id.to_string(),
        external_id: external_id.to_string(),
        configured,
        running: connection.running,
        connected: connection.connected,
        state: state.to_string(),
        connected_at_unix: connection.connected_at.map(|value| value.timestamp()),
        last_heartbeat_at_unix: connection.last_heartbeat_at.map(|value| value.timestamp()),
        last_event_at_unix: connection.last_event_at.map(|value| value.timestamp()),
        last_error: connection.last_error,
        last_error_at_unix: connection.last_error_at.map(|value| value.timestamp()),
        last_disconnect_at_unix: connection.last_disconnect_at.map(|value| value.timestamp()),
        last_disconnect_reason: connection.last_disconnect_reason,
        reconnect_count: connection.reconnect_count,
        observed_at_unix: Utc::now().timestamp(),
    }
}

fn success_response(data: ExternalModuleResponseData) -> HttpResponse {
    HttpResponse::Ok().json(json!({
        "errorCode": "0",
        "value": "成功",
        "code": 0,
        "message": "success",
        "data": data
    }))
}

fn success_json_response<T: Serialize>(data: T) -> HttpResponse {
    HttpResponse::Ok().json(json!({
        "errorCode": "0",
        "value": "成功",
        "code": 0,
        "message": "success",
        "data": data
    }))
}

fn fail_response(message: impl ToString) -> HttpResponse {
    let message = message.to_string();
    HttpResponse::Ok().json(json!({
        "errorCode": "1",
        "value": message,
        "code": 1,
        "message": message
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_credential_requires_typed_values() {
        let properties = Map::from_iter([
            (
                "baseUrl".to_string(),
                json!({"@type":"Data","value":"https://ilinkai.weixin.qq.com"}),
            ),
            (
                "gatewayUrl".to_string(),
                json!({
                    "@type":"ExternalServiceReference",
                    "url":"https://example.com/inbound",
                    "token":"binding-token"
                }),
            ),
            (
                "lowcodeForwardEnabled".to_string(),
                json!({"@type":"Data","value":true}),
            ),
            (
                "autoStart".to_string(),
                json!({"@type":"Data","value":true}),
            ),
        ]);

        let credential =
            build_credential(&properties, None, false).expect("typed values should parse");
        assert_eq!(
            credential.lowcode_ws_base_url.as_deref(),
            Some("https://example.com/inbound")
        );
        assert_eq!(
            credential.lowcode_ws_token.as_deref(),
            Some("binding-token")
        );
        assert_eq!(credential.lowcode_forward_enabled, Some(true));
        assert_eq!(credential.enabled, Some(true));
        assert_eq!(
            credential.api_base_url.as_deref(),
            Some("https://ilinkai.weixin.qq.com")
        );
        assert!(
            credential
                .outbound_token
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_some()
        );
    }

    #[test]
    fn build_credential_uses_explicit_gateway_token_over_reference_token() {
        let properties = Map::from_iter([
            (
                "gatewayUrl".to_string(),
                json!({
                    "@type":"ExternalServiceReference",
                    "url":"https://example.com/inbound",
                    "token":"binding-token"
                }),
            ),
            (
                "gatewayToken".to_string(),
                json!({"@type":"Data","value":"explicit-token"}),
            ),
        ]);

        let credential =
            build_credential(&properties, None, false).expect("typed values should parse");
        assert_eq!(
            credential.lowcode_ws_token.as_deref(),
            Some("explicit-token")
        );
    }

    #[test]
    fn build_credential_rejects_plain_gateway_url() {
        let properties = Map::from_iter([(
            "gatewayUrl".to_string(),
            Value::String("https://example.com/inbound".to_string()),
        )]);

        let error =
            build_credential(&properties, None, false).expect_err("plain string must be rejected");
        assert!(error.contains("gatewayUrl"));
        assert!(error.contains("ExternalServiceReference"));
    }

    #[test]
    fn resolve_target_external_id_reads_data_value() {
        let request = ExternalModuleUpdatePropertiesRequest::default();
        let properties = Map::from_iter([(
            "tenantId".to_string(),
            json!({"@type":"Data","value":"tenant-123"}),
        )]);

        let external_id =
            resolve_target_external_id(&request, &properties).expect("typed tenantId should parse");
        assert_eq!(external_id.as_deref(), Some("tenant-123"));
    }

    #[test]
    fn merge_response_properties_returns_typed_values() {
        let summary = crate::models::TenantSummary {
            tenant_id: "tenant-123".to_string(),
            logged_in: true,
            bot_token_masked: None,
            api_base_url: Some("https://ilinkai.weixin.qq.com".to_string()),
            account_id: None,
            user_id: None,
            lowcode_ws_base_url: Some("https://example.com/inbound".to_string()),
            lowcode_forward_enabled: Some(true),
            enabled: true,
            command_actions_configured: false,
            connection: crate::models::ConnectionStatus::default(),
        };
        let credential = crate::models::TenantCredential {
            bot_token: None,
            api_base_url: summary.api_base_url.clone(),
            account_id: None,
            user_id: None,
            sync_buf: None,
            lowcode_ws_base_url: summary.lowcode_ws_base_url.clone(),
            lowcode_ws_token: Some("gateway-token".to_string()),
            outbound_token: Some("outbound-token".to_string()),
            lowcode_forward_enabled: summary.lowcode_forward_enabled,
            enabled: Some(true),
            command_actions: None,
        };

        let properties = merge_response_properties(Map::new(), "tenant-123", &summary, &credential);
        assert_eq!(
            properties.get("tenantId"),
            Some(&json!({"@type":"Data","value":"tenant-123"}))
        );
        assert_eq!(
            properties.get("gatewayUrl"),
            Some(&json!({"@type":"ExternalServiceReference","url":"https://example.com/inbound"}))
        );
        assert_eq!(
            properties.get("lowcodeForwardEnabled"),
            Some(&json!({"@type":"Data","value":true}))
        );
        assert_eq!(
            properties.get("outboundToken"),
            Some(&json!({"@type":"Data","value":"outbound-token"}))
        );
    }
}
