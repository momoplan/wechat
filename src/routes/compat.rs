use crate::error::ServiceError;
use crate::models::SendTextMessageRequest;
use crate::state::{AppState, TenantContext};
use crate::wechat_api;
use actix_web::{HttpResponse, Scope, web};
use serde_json::{Map, Value};
use std::sync::Arc;

pub fn scope() -> Scope {
    web::scope("/common-tools/api/v1/channel")
        .route(
            "/message/sendBySession",
            web::post().to(send_message_by_session),
        )
        .route("/message/sendText", web::post().to(send_text_message))
}

async fn send_message_by_session(
    payload: web::Json<Value>,
    state: web::Data<Arc<AppState>>,
) -> Result<HttpResponse, ServiceError> {
    let payload = payload.into_inner();
    let body = require_object(&payload, "请求体")?;
    let tenant = resolve_tenant(&state, body).await?;
    let sdk_request = sdk_request(body)?;
    let session_key = require_string(
        sdk_request,
        &["sessionKey", "session_key"],
        "sdkRequest.sessionKey",
    )?;
    let text = require_string(sdk_request, &["text"], "sdkRequest.text")?;
    let context_token = optional_string(sdk_request, &["contextToken", "context_token"]);
    let data = wechat_api::send_session_text(
        state.get_ref(),
        &tenant,
        &session_key,
        &text,
        context_token.as_deref(),
    )
    .await?;
    Ok(HttpResponse::Ok().json(data))
}

async fn send_text_message(
    payload: web::Json<Value>,
    state: web::Data<Arc<AppState>>,
) -> Result<HttpResponse, ServiceError> {
    let payload = payload.into_inner();
    let body = require_object(&payload, "请求体")?;
    let tenant = resolve_tenant(&state, body).await?;
    let sdk_request = sdk_request(body)?;
    let data = wechat_api::send_text_message(
        state.get_ref(),
        &tenant,
        SendTextMessageRequest {
            to_user_id: optional_string(
                sdk_request,
                &["toUserId", "to_user_id", "userId", "user_id"],
            ),
            context_token: optional_string(sdk_request, &["contextToken", "context_token"]),
            text: require_string(sdk_request, &["text"], "sdkRequest.text")?,
        },
    )
    .await?;
    Ok(HttpResponse::Ok().json(data))
}

async fn resolve_tenant(
    state: &web::Data<Arc<AppState>>,
    body: &Map<String, Value>,
) -> Result<Arc<TenantContext>, ServiceError> {
    let tenant_id = require_string(body, &["tenantId"], "tenantId")?;
    state
        .get_tenant(&tenant_id)
        .ok_or_else(|| ServiceError::NotFound(format!("租户不存在: {tenant_id}")))
}

fn sdk_request(body: &Map<String, Value>) -> Result<&Map<String, Value>, ServiceError> {
    let sdk_request = body
        .get("sdkRequest")
        .or_else(|| body.get("sdk_request"))
        .ok_or_else(|| ServiceError::BadRequest("sdkRequest 不能为空".to_string()))?;
    require_object(sdk_request, "sdkRequest")
}

fn require_object<'a>(
    value: &'a Value,
    field_name: &str,
) -> Result<&'a Map<String, Value>, ServiceError> {
    value
        .as_object()
        .ok_or_else(|| ServiceError::BadRequest(format!("{field_name} 必须是对象")))
}

fn require_string(
    body: &Map<String, Value>,
    keys: &[&str],
    field_name: &str,
) -> Result<String, ServiceError> {
    optional_string(body, keys)
        .ok_or_else(|| ServiceError::BadRequest(format!("{field_name} 不能为空")))
}

fn optional_string(body: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        body.get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    })
}
