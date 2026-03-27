use crate::error::ServiceError;
use crate::models::{TenantCredential, TenantSummary};
use crate::state::AppState;
use crate::wechat_api;
use actix_web::{HttpResponse, Responder, Scope, web};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use tracing::{info, warn};

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
    #[serde(default)]
    auto_start: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct LoginStatusRequest {
    #[serde(default)]
    qrcode: Option<String>,
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
    tenant.prune_finished_login_worker().await;
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
    let _ = state.stop_login_worker(&tenant).await;
    tenant.clear_login_session().await;

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
    let _ = state.stop_login_worker(&tenant).await;
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
    let request = payload.into_inner();
    let auto_start = request.auto_start.unwrap_or(true);
    info!(
        tenant_id = %tenant_id,
        base_url_override = ?request.base_url,
        auto_start,
        "收到微信登录二维码申请"
    );
    let tenant = state
        .get_tenant(&tenant_id)
        .ok_or_else(|| ServiceError::NotFound(format!("租户不存在: {tenant_id}")))?;
    let response =
        wechat_api::fetch_login_qrcode(state.get_ref(), &tenant, request.base_url.as_deref())
            .await?;
    let qr_image_data_url =
        wechat_api::normalize_login_qr_image_data_url(&response.qrcode_img_content)?;
    info!(
        tenant_id = %tenant_id,
        qrcode = %wechat_api::mask_identifier(&response.qrcode),
        ret = response.ret,
        "微信登录二维码申请成功"
    );

    let login_base_url = request
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .or_else(|| {
            tenant
                .credential
                .try_read()
                .ok()
                .and_then(|credential| credential.api_base_url.clone())
        })
        .unwrap_or_else(|| state.config.wechat.base_url.clone());
    state
        .start_login_worker(
            tenant.clone(),
            response.qrcode.clone(),
            login_base_url.clone(),
            auto_start,
        )
        .await?;

    Ok(HttpResponse::Ok().json(json!({
        "tenant_id": tenant_id,
        "qrcode": response.qrcode,
        "url": response.qrcode_img_content,
        "login_url": response.qrcode_img_content,
        "qr_image_data_url": qr_image_data_url,
        "ret": response.ret,
        "server_polling": true,
        "auto_start": auto_start,
        "login_status": {
            "qrcode": response.qrcode,
            "base_url": login_base_url,
            "status": "pending",
            "confirmed": false
        }
    })))
}

async fn check_login_status(
    path: web::Path<String>,
    state: web::Data<Arc<AppState>>,
    payload: web::Json<LoginStatusRequest>,
) -> Result<HttpResponse, ServiceError> {
    let tenant_id = path.into_inner();
    let tenant = state
        .get_tenant(&tenant_id)
        .ok_or_else(|| ServiceError::NotFound(format!("租户不存在: {tenant_id}")))?;
    tenant.prune_finished_login_worker().await;
    let request = payload.into_inner();
    let requested_qrcode = request.qrcode.clone().unwrap_or_default();
    info!(
        tenant_id = %tenant_id,
        qrcode = %wechat_api::mask_identifier(&requested_qrcode),
        auto_start = request.auto_start.unwrap_or(true),
        base_url_override = ?request.base_url,
        "收到微信扫码状态查询请求"
    );

    if let Some(session) = tenant.get_login_session().await {
        let qrcode_matched =
            requested_qrcode.trim().is_empty() || requested_qrcode == session.qrcode;
        if qrcode_matched {
            if let Some(auto_start) = request.auto_start {
                let _ = tenant
                    .update_login_session(&session.qrcode, |current| {
                        current.auto_start = auto_start;
                    })
                    .await;
            }
            let current_session = tenant.get_login_session().await.unwrap_or(session);
            info!(
                tenant_id = %tenant_id,
                qrcode = %wechat_api::mask_identifier(&current_session.qrcode),
                status = %current_session.status,
                confirmed = current_session.confirmed,
                "返回服务端维护的微信扫码状态"
            );
            let mut body = json!({
                "tenant_id": tenant_id,
                "status": current_session.status,
                "confirmed": current_session.confirmed,
                "server_polling": true,
                "qrcode": current_session.qrcode,
                "base_url": current_session.base_url,
                "auto_start": current_session.auto_start,
                "updated_at": current_session.updated_at,
                "completed_at": current_session.completed_at,
                "last_error": current_session.last_error
            });
            if current_session.confirmed {
                body["tenant"] = serde_json::to_value(tenant.summary().await)
                    .map_err(|err| ServiceError::Internal(format!("序列化租户状态失败: {err}")))?;
            }
            return Ok(HttpResponse::Ok().json(body));
        }

        warn!(
            tenant_id = %tenant_id,
            requested_qrcode = %wechat_api::mask_identifier(&requested_qrcode),
            tracked_qrcode = %wechat_api::mask_identifier(&session.qrcode),
            "请求的二维码与服务端当前跟踪会话不一致，返回当前会话状态"
        );

        let mut body = json!({
            "tenant_id": tenant_id,
            "status": session.status,
            "confirmed": session.confirmed,
            "server_polling": true,
            "requested_qrcode_matched": false,
            "qrcode": session.qrcode,
            "base_url": session.base_url,
            "auto_start": session.auto_start,
            "updated_at": session.updated_at,
            "completed_at": session.completed_at,
            "last_error": session.last_error
        });
        if session.confirmed {
            body["tenant"] = serde_json::to_value(tenant.summary().await)
                .map_err(|err| ServiceError::Internal(format!("序列化租户状态失败: {err}")))?;
        }
        return Ok(HttpResponse::Ok().json(body));
    }

    if requested_qrcode.trim().is_empty() {
        return Err(ServiceError::BadRequest(
            "当前没有服务端跟踪中的二维码，且请求缺少 qrcode".to_string(),
        ));
    }

    let response = wechat_api::poll_login_status(
        state.get_ref(),
        &tenant,
        &requested_qrcode,
        request.base_url.as_deref(),
    )
    .await?;
    info!(
        tenant_id = %tenant_id,
        qrcode = %wechat_api::mask_identifier(&requested_qrcode),
        status = %response.status,
        ilink_bot_id = ?response.ilink_bot_id,
        ilink_user_id = ?response.ilink_user_id,
        "兼容模式微信扫码状态查询完成"
    );

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
        let _ = state.stop_login_worker(&tenant).await;
        tenant.clear_login_session().await;

        let auto_start = request.auto_start.unwrap_or(true);
        if auto_start && tenant.credential.read().await.is_enabled() {
            match start_worker_internal(state.get_ref().clone(), tenant.clone()).await {
                Ok(()) => info!(
                    tenant_id = %tenant_id,
                    account_id = ?response.ilink_bot_id,
                    user_id = ?response.ilink_user_id,
                    auto_start,
                    "兼容模式微信扫码确认成功并已启动租户轮询"
                ),
                Err(err) => warn!(
                    tenant_id = %tenant_id,
                    account_id = ?response.ilink_bot_id,
                    user_id = ?response.ilink_user_id,
                    auto_start,
                    error = %err,
                    "兼容模式微信扫码确认成功，但启动租户轮询失败"
                ),
            }
        }

        return Ok(HttpResponse::Ok().json(json!({
            "tenant_id": tenant_id,
            "status": response.status,
            "confirmed": true,
            "server_polling": false,
            "tenant": tenant.summary().await
        })));
    }

    Ok(HttpResponse::Ok().json(json!({
        "tenant_id": tenant_id,
        "status": response.status,
        "confirmed": false,
        "server_polling": false
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
    credential.outbound_token = normalize_optional(credential.outbound_token.take());
    credential.ensure_outbound_token();

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
