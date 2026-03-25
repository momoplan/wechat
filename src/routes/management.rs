use crate::error::ServiceError;
use crate::models::{TenantCredential, TenantSummary};
use crate::state::AppState;
use crate::wechat_api;
use actix_web::{HttpResponse, Responder, Scope, web};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::sync::Arc;

#[derive(Debug, Deserialize)]
struct EventQuery {
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct LoginQrRequest {
    #[serde(default, alias = "baseUrl")]
    base_url: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct LoginStatusRequest {
    qrcode: String,
    #[serde(default, alias = "baseUrl")]
    base_url: Option<String>,
    #[serde(default)]
    auto_start: Option<bool>,
}

pub fn scope() -> Scope {
    web::scope("")
        .service(super::compat::scope())
        .service(super::external_module::scope())
        .route("/health", web::get().to(health))
        .route("/tenants", web::get().to(list_tenants))
        .route("/tenants/{tenant_id}", web::get().to(get_tenant))
        .route("/tenants/{tenant_id}", web::put().to(upsert_tenant))
        .route("/tenants/{tenant_id}", web::delete().to(delete_tenant))
        .route(
            "/tenants/{tenant_id}/connection/start",
            web::post().to(start_connection),
        )
        .route(
            "/tenants/{tenant_id}/connection/stop",
            web::post().to(stop_connection),
        )
        .route(
            "/tenants/{tenant_id}/events",
            web::get().to(list_tenant_events),
        )
        .route(
            "/tenants/{tenant_id}/login/qrcode",
            web::post().to(create_login_qrcode),
        )
        .route(
            "/tenants/{tenant_id}/login/status",
            web::post().to(check_login_status),
        )
}

async fn health() -> impl Responder {
    HttpResponse::Ok().json(json!({"status": "ok"}))
}

async fn list_tenants(state: web::Data<Arc<AppState>>) -> Result<HttpResponse, ServiceError> {
    let tenants = state.list_tenants().await;
    Ok(HttpResponse::Ok().json(tenants))
}

async fn get_tenant(
    path: web::Path<String>,
    state: web::Data<Arc<AppState>>,
) -> Result<HttpResponse, ServiceError> {
    let tenant_id = path.into_inner();
    let tenant = state
        .get_tenant(&tenant_id)
        .ok_or_else(|| ServiceError::NotFound(format!("租户不存在: {tenant_id}")))?;
    tenant.prune_finished_worker().await;
    Ok(HttpResponse::Ok().json(tenant.summary().await))
}

async fn upsert_tenant(
    path: web::Path<String>,
    state: web::Data<Arc<AppState>>,
    payload: web::Json<TenantCredential>,
) -> Result<HttpResponse, ServiceError> {
    let tenant_id = path.into_inner();
    let mut credential = payload.into_inner();
    normalize_credential(&mut credential)?;

    let (tenant, created) = state.upsert_tenant(&tenant_id, credential).await?;
    tenant.prune_finished_worker().await;

    let should_run = {
        let credential = tenant.credential.read().await.clone();
        credential.is_enabled() && credential.is_logged_in()
    };
    let was_running = {
        let guard = tenant.worker.lock().await;
        guard.is_some()
    };
    if was_running {
        stop_worker_internal(state.get_ref(), &tenant).await;
    }
    if should_run {
        let _ = start_worker_internal(state.get_ref().clone(), tenant.clone()).await;
    }

    let summary: TenantSummary = tenant.summary().await;
    Ok(HttpResponse::Ok().json(json!({
        "created": created,
        "enabled": summary.enabled,
        "logged_in": summary.logged_in,
        "tenant": summary
    })))
}

async fn delete_tenant(
    path: web::Path<String>,
    state: web::Data<Arc<AppState>>,
) -> Result<HttpResponse, ServiceError> {
    let tenant_id = path.into_inner();
    let tenant = state
        .remove_tenant(&tenant_id)
        .await?
        .ok_or_else(|| ServiceError::NotFound(format!("租户不存在: {tenant_id}")))?;
    stop_worker_internal(state.get_ref(), &tenant).await;

    Ok(HttpResponse::Ok().json(json!({
        "deleted": true,
        "tenant_id": tenant_id
    })))
}

async fn start_connection(
    path: web::Path<String>,
    state: web::Data<Arc<AppState>>,
) -> Result<HttpResponse, ServiceError> {
    let tenant_id = path.into_inner();
    let tenant = state.set_tenant_enabled(&tenant_id, true).await?;
    tenant.prune_finished_worker().await;

    let logged_in = tenant.credential.read().await.is_logged_in();
    if logged_in {
        start_worker_internal(state.get_ref().clone(), tenant.clone()).await?;
    }

    Ok(HttpResponse::Ok().json(json!({
        "started": logged_in,
        "login_required": !logged_in,
        "tenant": tenant.summary().await
    })))
}

async fn stop_connection(
    path: web::Path<String>,
    state: web::Data<Arc<AppState>>,
) -> Result<HttpResponse, ServiceError> {
    let tenant_id = path.into_inner();
    let tenant = state.set_tenant_enabled(&tenant_id, false).await?;
    let stopped = stop_worker_internal(state.get_ref(), &tenant).await;

    Ok(HttpResponse::Ok().json(json!({
        "stopped": stopped,
        "tenant": tenant.summary().await
    })))
}

async fn list_tenant_events(
    path: web::Path<String>,
    query: web::Query<EventQuery>,
    state: web::Data<Arc<AppState>>,
) -> Result<HttpResponse, ServiceError> {
    let tenant_id = path.into_inner();
    let tenant = state
        .get_tenant(&tenant_id)
        .ok_or_else(|| ServiceError::NotFound(format!("租户不存在: {tenant_id}")))?;

    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let events = tenant.list_events(limit).await;

    Ok(HttpResponse::Ok().json(json!({
        "tenant_id": tenant_id,
        "count": events.len(),
        "events": events
    })))
}

async fn create_login_qrcode(
    path: web::Path<String>,
    state: web::Data<Arc<AppState>>,
    payload: web::Json<LoginQrRequest>,
) -> Result<HttpResponse, ServiceError> {
    let tenant_id = path.into_inner();
    let tenant = state
        .get_tenant(&tenant_id)
        .ok_or_else(|| ServiceError::NotFound(format!("租户不存在: {tenant_id}")))?;
    let response =
        wechat_api::fetch_login_qrcode(state.get_ref(), &tenant, payload.base_url.as_deref())
            .await?;
    let qr_image_data_url =
        wechat_api::normalize_login_qr_image_data_url(&response.qrcode_img_content)?;

    Ok(HttpResponse::Ok().json(json!({
        "tenant_id": tenant_id,
        "qrcode": response.qrcode,
        "url": response.qrcode_img_content,
        "login_url": response.qrcode_img_content,
        "qr_image_data_url": qr_image_data_url,
        "ret": response.ret
    })))
}

async fn check_login_status(
    path: web::Path<String>,
    state: web::Data<Arc<AppState>>,
    payload: web::Json<LoginStatusRequest>,
) -> Result<HttpResponse, ServiceError> {
    let tenant_id = path.into_inner();
    let existing = state
        .get_tenant(&tenant_id)
        .ok_or_else(|| ServiceError::NotFound(format!("租户不存在: {tenant_id}")))?;
    let request = payload.into_inner();
    let response = wechat_api::poll_login_status(
        state.get_ref(),
        &existing,
        &request.qrcode,
        request.base_url.as_deref(),
    )
    .await?;

    if response.status == "confirmed" {
        let bot_token = response
            .bot_token
            .as_deref()
            .ok_or_else(|| ServiceError::Upstream("扫码确认成功但缺少 bot_token".to_string()))?;
        let base_url = response
            .baseurl
            .as_deref()
            .or(request.base_url.as_deref())
            .unwrap_or(state.config.wechat.base_url.as_str());

        let tenant = state
            .update_login_result(
                &tenant_id,
                bot_token,
                base_url,
                response.ilink_bot_id.as_deref(),
                response.ilink_user_id.as_deref(),
            )
            .await?;

        let auto_start = request.auto_start.unwrap_or(true);
        if auto_start && tenant.credential.read().await.is_enabled() {
            let _ = start_worker_internal(state.get_ref().clone(), tenant.clone()).await;
        }

        return Ok(HttpResponse::Ok().json(json!({
            "tenant_id": tenant_id,
            "status": response.status,
            "confirmed": true,
            "tenant": tenant.summary().await
        })));
    }

    Ok(HttpResponse::Ok().json(json!({
        "tenant_id": tenant_id,
        "status": response.status,
        "confirmed": false
    })))
}

async fn start_worker_internal(
    app_state: Arc<AppState>,
    tenant: Arc<crate::state::TenantContext>,
) -> Result<(), ServiceError> {
    if !app_state.start_tenant_worker(tenant.clone()).await? {
        return Ok(());
    }
    Ok(())
}

async fn stop_worker_internal(
    app_state: &Arc<AppState>,
    tenant: &Arc<crate::state::TenantContext>,
) -> bool {
    app_state.stop_tenant_worker(tenant).await
}

pub(crate) fn normalize_credential(credential: &mut TenantCredential) -> Result<(), ServiceError> {
    credential.bot_token = normalize_optional(credential.bot_token.take());
    credential.api_base_url = normalize_optional(credential.api_base_url.take());
    credential.account_id = normalize_optional(credential.account_id.take());
    credential.user_id = normalize_optional(credential.user_id.take());
    credential.sync_buf = normalize_optional(credential.sync_buf.take());
    credential.lowcode_ws_base_url = normalize_optional(credential.lowcode_ws_base_url.take());
    credential.lowcode_ws_token = normalize_optional(credential.lowcode_ws_token.take());

    if credential.lowcode_forward_enabled.unwrap_or(false)
        && credential.lowcode_ws_base_url.is_none()
    {
        return Err(ServiceError::BadRequest(
            "启用 lowcode 转发时必须配置 gateway_url".to_string(),
        ));
    }

    Ok(())
}

pub(crate) fn normalize_optional(value: Option<String>) -> Option<String> {
    value
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
}

pub(crate) fn merge_properties(
    properties: Option<Map<String, Value>>,
    current_properties: Option<Map<String, Value>>,
) -> Map<String, Value> {
    let mut merged = current_properties.unwrap_or_default();
    if let Some(properties) = properties {
        for (key, value) in properties {
            merged.insert(key, value);
        }
    }
    merged
}
