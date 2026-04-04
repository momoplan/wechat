use crate::error::ServiceError;
use crate::models::{SendMediaMessageRequest, SendTextMessageRequest, TenantCredential};
use crate::state::{AppState, TenantContext};
use aes::Aes128;
use aes::cipher::{BlockEncryptMut, KeyInit, block_padding::Pkcs7};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use ecb::Encryptor;
use hex::encode as hex_encode;
use image::{DynamicImage, ImageFormat, Luma};
use qrcode::QrCode;
use rand::Rng as _;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

type Aes128EcbEnc = Encryptor<Aes128>;

#[derive(Debug, Deserialize, Clone)]
pub struct LoginQrResponse {
    pub qrcode: String,
    #[serde(rename = "qrcode_img_content")]
    pub qrcode_img_content: String,
    pub ret: i64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LoginStatusResponse {
    pub status: String,
    #[serde(default)]
    pub bot_token: Option<String>,
    #[serde(default)]
    pub baseurl: Option<String>,
    #[serde(default, rename = "ilink_bot_id")]
    pub ilink_bot_id: Option<String>,
    #[serde(default, rename = "ilink_user_id")]
    pub ilink_user_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GetUpdatesResponse {
    #[serde(default)]
    pub ret: Option<i64>,
    #[serde(default)]
    pub errmsg: Option<String>,
    #[serde(default)]
    pub msgs: Vec<Value>,
    #[serde(default, rename = "get_updates_buf")]
    pub get_updates_buf: Option<String>,
}

const DEFAULT_CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";
const CDN_UPLOAD_RETRIES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaKind {
    Image,
    Audio,
    Video,
    File,
}

impl MediaKind {
    fn from_hint(value: Option<&str>) -> Option<Self> {
        match value.map(str::trim).filter(|value| !value.is_empty())? {
            value if value.eq_ignore_ascii_case("image") => Some(Self::Image),
            value if value.eq_ignore_ascii_case("audio") || value.eq_ignore_ascii_case("voice") => {
                Some(Self::Audio)
            }
            value if value.eq_ignore_ascii_case("video") => Some(Self::Video),
            value if value.eq_ignore_ascii_case("file") => Some(Self::File),
            _ => None,
        }
    }

    fn upload_media_type(self) -> i64 {
        match self {
            Self::Image => 1,
            Self::Audio => 3,
            Self::Video => 2,
            Self::File => 3,
        }
    }

    fn message_item_type(self) -> i64 {
        match self {
            Self::Image => 2,
            Self::Audio => 3,
            Self::Video => 5,
            Self::File => 4,
        }
    }
}

#[derive(Debug)]
struct MediaPayload {
    bytes: Vec<u8>,
    file_name: String,
    media_kind: MediaKind,
}

#[derive(Debug)]
struct UploadedMedia {
    download_encrypted_query_param: String,
    aes_key_hex: String,
    plaintext_size: usize,
    ciphertext_size: usize,
}

fn normalize_base_url(value: &str) -> String {
    let trimmed = value.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        "https://ilinkai.weixin.qq.com".to_string()
    } else {
        trimmed.to_string()
    }
}

fn random_wechat_uin() -> String {
    let value = rand::rng().random::<u32>().to_string();
    BASE64_STANDARD.encode(value)
}

pub fn classify_login_qr_content(value: &str) -> &'static str {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "empty";
    }
    if trimmed.starts_with("data:image/") {
        return "data_url";
    }
    if looks_like_base64(trimmed) {
        return "base64";
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return "url";
    }
    "unknown"
}

pub fn preview_login_qr_content(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "(empty)".to_string();
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return trimmed.chars().take(120).collect();
    }
    let prefix: String = trimmed.chars().take(24).collect();
    format!("{prefix}...")
}

pub fn normalize_login_qr_image_data_url(value: &str) -> Result<String, ServiceError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ServiceError::Upstream(
            "微信未返回可用的二维码内容".to_string(),
        ));
    }

    if trimmed.starts_with("data:image/") {
        return Ok(trimmed.to_string());
    }

    if looks_like_base64(trimmed) {
        return Ok(format!("data:image/png;base64,{trimmed}"));
    }

    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return build_png_qr_data_url(trimmed);
    }

    Err(ServiceError::Upstream(format!(
        "未识别的二维码内容格式: {trimmed}"
    )))
}

fn looks_like_base64(value: &str) -> bool {
    !value.is_empty()
        && value.len().is_multiple_of(4)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
}

fn build_png_qr_data_url(content: &str) -> Result<String, ServiceError> {
    let qrcode = QrCode::new(content.as_bytes())
        .map_err(|err| ServiceError::Internal(format!("生成二维码失败: {err}")))?;
    let image = qrcode
        .render::<Luma<u8>>()
        .min_dimensions(280, 280)
        .quiet_zone(true)
        .build();
    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageLuma8(image)
        .write_to(&mut output, ImageFormat::Png)
        .map_err(|err| ServiceError::Internal(format!("编码二维码图片失败: {err}")))?;
    let encoded = BASE64_STANDARD.encode(output.into_inner());
    Ok(format!("data:image/png;base64,{encoded}"))
}

fn build_headers(token: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Content-Type", "application/json".to_string()),
        ("AuthorizationType", "ilink_bot_token".to_string()),
        ("Authorization", format!("Bearer {token}")),
        ("X-WECHAT-UIN", random_wechat_uin()),
    ]
}

async fn parse_api_json(resp: reqwest::Response, endpoint: &str) -> Result<Value, ServiceError> {
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|err| ServiceError::Upstream(format!("读取微信响应失败: {err}")))?;
    if !status.is_success() {
        return Err(ServiceError::Upstream(format!(
            "{endpoint} 返回 HTTP {}: {}",
            status.as_u16(),
            text
        )));
    }

    serde_json::from_str::<Value>(&text).map_err(|err| {
        ServiceError::Upstream(format!("解析微信响应 JSON 失败: {err}; body={text}"))
    })
}

fn mask_token(value: &str) -> String {
    let trimmed = value.trim();
    let len = trimmed.chars().count();
    if len <= 12 {
        return "******".to_string();
    }
    let prefix: String = trimmed.chars().take(6).collect();
    let suffix: String = trimmed
        .chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}******{suffix}")
}

pub fn mask_identifier(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "(empty)".to_string();
    }

    let len = trimmed.chars().count();
    if len <= 12 {
        return mask_token(trimmed);
    }

    let prefix: String = trimmed.chars().take(4).collect();
    let suffix: String = trimmed
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}******{suffix}")
}

fn redact_sendmessage_payload(payload: &Value) -> Value {
    let mut redacted = payload.clone();
    if let Some(value) = redacted.pointer_mut("/msg/context_token") {
        if let Some(token) = value.as_str() {
            *value = Value::String(mask_token(token));
        }
    }
    redacted
}

fn build_base_info() -> Value {
    json!({
        "channel_version": "0.1.0"
    })
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

async fn resolve_context_token(
    tenant: &Arc<TenantContext>,
    user_id: &str,
    context_token: Option<&str>,
) -> Result<String, ServiceError> {
    if let Some(value) = context_token
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
    {
        Ok(value)
    } else if let Some(value) = tenant.get_context_token(user_id).await {
        Ok(value)
    } else {
        Err(ServiceError::BadRequest(
            "缺少 contextToken，无法回消息".to_string(),
        ))
    }
}

fn build_sendmessage_body(user_id: &str, context_token: &str, item_list: Vec<Value>) -> Value {
    json!({
        "msg": {
            "from_user_id": "",
            "to_user_id": user_id,
            "client_id": format!("lowcode-wechat-{}", uuid::Uuid::new_v4()),
            "message_type": 2,
            "message_state": 2,
            "item_list": item_list,
            "context_token": context_token
        },
        "base_info": build_base_info()
    })
}

async fn send_message_body(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    token: &str,
    base_url: &str,
    user_id: &str,
    body: Value,
) -> Result<Value, ServiceError> {
    let endpoint = format!("{base_url}/ilink/bot/sendmessage");
    let log_payload = redact_sendmessage_payload(&body);
    info!(
        tenant_id = %tenant.tenant_id,
        user_id,
        endpoint = %endpoint,
        payload = %log_payload,
        "调用微信 sendmessage 请求"
    );

    let mut request = state.http_client.post(&endpoint).json(&body);
    for (key, value) in build_headers(token) {
        request = request.header(key, value);
    }

    let response = request
        .send()
        .await
        .map_err(|err| ServiceError::Upstream(format!("发送微信消息失败: {err}")))?;
    let status = response.status();
    let raw_body = response
        .text()
        .await
        .map_err(|err| ServiceError::Upstream(format!("读取微信响应失败: {err}")))?;
    info!(
        tenant_id = %tenant.tenant_id,
        user_id,
        endpoint = %endpoint,
        status = %status,
        response_body = %raw_body,
        "微信 sendmessage 响应"
    );

    let body = if !status.is_success() {
        return Err(ServiceError::Upstream(format!(
            "sendmessage 返回 HTTP {}: {}",
            status.as_u16(),
            raw_body
        )));
    } else if raw_body.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str::<Value>(&raw_body).map_err(|err| {
            ServiceError::Upstream(format!("解析微信响应 JSON 失败: {err}; body={raw_body}"))
        })?
    };
    ensure_success(&body, "sendmessage")?;
    Ok(body)
}

fn aes_ecb_padded_size(plaintext_size: usize) -> usize {
    ((plaintext_size / 16) + 1) * 16
}

fn encrypt_aes_ecb(plaintext: &[u8], key: &[u8; 16]) -> Result<Vec<u8>, ServiceError> {
    let mut buffer = plaintext.to_vec();
    let original_len = buffer.len();
    buffer.resize(original_len + 16, 0);
    let ciphertext = Aes128EcbEnc::new(key.into())
        .encrypt_padded_mut::<Pkcs7>(&mut buffer, original_len)
        .map_err(|err| ServiceError::Internal(format!("AES-128-ECB 加密失败: {err}")))?;
    Ok(ciphertext.to_vec())
}

fn build_cdn_upload_url(upload_param: &str, filekey: &str) -> String {
    format!(
        "{DEFAULT_CDN_BASE_URL}/upload?encrypted_query_param={}&filekey={}",
        urlencoding::encode(upload_param),
        urlencoding::encode(filekey)
    )
}

async fn get_upload_url(
    state: &Arc<AppState>,
    token: &str,
    base_url: &str,
    to_user_id: &str,
    media_kind: MediaKind,
    plaintext: &[u8],
    filekey: &str,
    aes_key_hex: &str,
) -> Result<String, ServiceError> {
    let endpoint = format!("{base_url}/ilink/bot/getuploadurl");
    let body = json!({
        "filekey": filekey,
        "media_type": media_kind.upload_media_type(),
        "to_user_id": to_user_id,
        "rawsize": plaintext.len(),
        "rawfilemd5": format!("{:x}", md5::compute(plaintext)),
        "filesize": aes_ecb_padded_size(plaintext.len()),
        "no_need_thumb": true,
        "aeskey": aes_key_hex,
        "base_info": build_base_info()
    });

    let mut request = state.http_client.post(&endpoint).json(&body);
    for (key, value) in build_headers(token) {
        request = request.header(key, value);
    }

    let response = request
        .send()
        .await
        .map_err(|err| ServiceError::Upstream(format!("获取微信上传地址失败: {err}")))?;
    let body = parse_api_json(response, "getuploadurl").await?;
    ensure_success(&body, "getuploadurl")?;
    body.get("upload_param")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| ServiceError::Upstream("getuploadurl 未返回 upload_param".to_string()))
}

async fn upload_media_to_cdn(
    state: &Arc<AppState>,
    plaintext: &[u8],
    upload_param: &str,
    filekey: &str,
    aes_key: &[u8; 16],
) -> Result<String, ServiceError> {
    let ciphertext = encrypt_aes_ecb(plaintext, aes_key)?;
    let endpoint = build_cdn_upload_url(upload_param, filekey);
    let mut last_error: Option<String> = None;

    for attempt in 1..=CDN_UPLOAD_RETRIES {
        let response = state
            .http_client
            .post(&endpoint)
            .header("Content-Type", "application/octet-stream")
            .body(ciphertext.clone())
            .send()
            .await;

        match response {
            Ok(resp) if resp.status().is_success() => {
                if let Some(value) = resp
                    .headers()
                    .get("x-encrypted-param")
                    .and_then(|value| value.to_str().ok())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    return Ok(value.to_string());
                }
                return Err(ServiceError::Upstream(
                    "CDN 上传成功但缺少 x-encrypted-param".to_string(),
                ));
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp
                    .text()
                    .await
                    .unwrap_or_else(|_| "(读取 CDN 响应失败)".to_string());
                let message = format!("CDN 上传失败 HTTP {}: {}", status.as_u16(), body);
                if status.is_client_error() {
                    return Err(ServiceError::Upstream(message));
                }
                last_error = Some(message);
            }
            Err(err) => {
                last_error = Some(format!("CDN 上传请求失败: {err}"));
            }
        }

        if attempt < CDN_UPLOAD_RETRIES {
            warn!(attempt, "CDN 上传失败，准备重试");
        }
    }

    Err(ServiceError::Upstream(
        last_error.unwrap_or_else(|| "CDN 上传失败".to_string()),
    ))
}

fn infer_media_kind_from_name(name: &str) -> MediaKind {
    match Path::new(name)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp") => MediaKind::Image,
        Some("amr" | "wav" | "mp3" | "m4a" | "aac" | "ogg" | "opus" | "silk") => {
            MediaKind::Audio
        }
        Some("mp4" | "mov" | "webm" | "mkv" | "avi") => MediaKind::Video,
        _ => MediaKind::File,
    }
}

fn infer_media_kind_from_content_type(value: Option<&str>) -> Option<MediaKind> {
    let content_type = value?
        .split(';')
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    if content_type.starts_with("image/") {
        Some(MediaKind::Image)
    } else if content_type.starts_with("audio/") {
        Some(MediaKind::Audio)
    } else if content_type.starts_with("video/") {
        Some(MediaKind::Video)
    } else {
        Some(MediaKind::File)
    }
}

fn file_name_from_url(url: &reqwest::Url) -> Option<String> {
    url.path_segments()
        .and_then(|segments| segments.last())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

async fn load_media_payload(
    state: &Arc<AppState>,
    media_url: Option<&str>,
    media_path: Option<&str>,
    file_name: Option<&str>,
    media_type: Option<&str>,
) -> Result<MediaPayload, ServiceError> {
    if let Some(url) = media_url.map(str::trim).filter(|value| !value.is_empty()) {
        let parsed = reqwest::Url::parse(url)
            .map_err(|err| ServiceError::BadRequest(format!("media_url 非法: {err}")))?;
        let response = state
            .http_client
            .get(parsed.clone())
            .send()
            .await
            .map_err(|err| ServiceError::Upstream(format!("下载媒体文件失败: {err}")))?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "(读取下载响应失败)".to_string());
            return Err(ServiceError::Upstream(format!(
                "下载媒体文件失败 HTTP {}: {}",
                status.as_u16(),
                body
            )));
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
        let bytes = response
            .bytes()
            .await
            .map_err(|err| ServiceError::Upstream(format!("读取媒体文件失败: {err}")))?;
        let fallback_name = file_name
            .map(ToString::to_string)
            .or_else(|| file_name_from_url(&parsed))
            .unwrap_or_else(|| "media.bin".to_string());
        let kind = MediaKind::from_hint(media_type)
            .or_else(|| infer_media_kind_from_content_type(content_type.as_deref()))
            .unwrap_or_else(|| infer_media_kind_from_name(&fallback_name));
        return Ok(MediaPayload {
            bytes: bytes.to_vec(),
            file_name: fallback_name,
            media_kind: kind,
        });
    }

    let Some(path_str) = media_path.map(str::trim).filter(|value| !value.is_empty()) else {
        return Err(ServiceError::BadRequest(
            "media_url/media_path 不能同时为空".to_string(),
        ));
    };

    let path = PathBuf::from(path_str);
    let file_bytes = tokio::fs::read(&path)
        .await
        .map_err(|err| ServiceError::BadRequest(format!("读取媒体文件失败: {err}")))?;
    let fallback_name = file_name
        .map(ToString::to_string)
        .or_else(|| {
            path.file_name()
                .and_then(|value| value.to_str())
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| "media.bin".to_string());
    let kind = MediaKind::from_hint(media_type)
        .unwrap_or_else(|| infer_media_kind_from_name(&fallback_name));
    Ok(MediaPayload {
        bytes: file_bytes,
        file_name: fallback_name,
        media_kind: kind,
    })
}

async fn upload_media(
    state: &Arc<AppState>,
    token: &str,
    base_url: &str,
    user_id: &str,
    payload: &MediaPayload,
) -> Result<UploadedMedia, ServiceError> {
    let filekey = hex_encode(rand::rng().random::<[u8; 16]>());
    let aes_key = rand::rng().random::<[u8; 16]>();
    let aes_key_hex = hex_encode(aes_key);
    let upload_param = get_upload_url(
        state,
        token,
        base_url,
        user_id,
        payload.media_kind,
        &payload.bytes,
        &filekey,
        &aes_key_hex,
    )
    .await?;
    let download_encrypted_query_param =
        upload_media_to_cdn(state, &payload.bytes, &upload_param, &filekey, &aes_key).await?;
    Ok(UploadedMedia {
        download_encrypted_query_param,
        aes_key_hex,
        plaintext_size: payload.bytes.len(),
        ciphertext_size: aes_ecb_padded_size(payload.bytes.len()),
    })
}

fn build_media_item(payload: &MediaPayload, uploaded: &UploadedMedia) -> Value {
    let media = json!({
        "encrypt_query_param": uploaded.download_encrypted_query_param,
        "aes_key": BASE64_STANDARD.encode(hex::decode(&uploaded.aes_key_hex).unwrap_or_default()),
        "encrypt_type": 1
    });

    match payload.media_kind {
        MediaKind::Image => json!({
            "type": payload.media_kind.message_item_type(),
            "image_item": {
                "media": media,
                "mid_size": uploaded.ciphertext_size
            }
        }),
        MediaKind::Audio => json!({
            "type": payload.media_kind.message_item_type(),
            "voice_item": {
                "media": media
            }
        }),
        MediaKind::Video => json!({
            "type": payload.media_kind.message_item_type(),
            "video_item": {
                "media": media,
                "video_size": uploaded.ciphertext_size
            }
        }),
        MediaKind::File => json!({
            "type": payload.media_kind.message_item_type(),
            "file_item": {
                "media": media,
                "file_name": payload.file_name,
                "len": uploaded.plaintext_size.to_string()
            }
        }),
    }
}

fn ensure_success(body: &Value, endpoint: &str) -> Result<(), ServiceError> {
    let ret = body.get("ret").and_then(Value::as_i64).unwrap_or(0);
    if ret == 0 {
        return Ok(());
    }

    let errmsg = body
        .get("errmsg")
        .and_then(Value::as_str)
        .unwrap_or("unknown error");
    Err(ServiceError::Upstream(format!(
        "{endpoint} 返回错误 ret={ret} errmsg={errmsg}"
    )))
}

fn ensure_tenant_active(credential: &TenantCredential) -> Result<(), ServiceError> {
    if !credential.is_enabled() {
        return Err(ServiceError::BadRequest("租户已停用".to_string()));
    }
    if !credential.is_logged_in() {
        return Err(ServiceError::BadRequest("租户尚未扫码登录".to_string()));
    }
    Ok(())
}

pub async fn fetch_login_qrcode(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    base_url_override: Option<&str>,
) -> Result<LoginQrResponse, ServiceError> {
    let tenant_base = tenant.credential.read().await.api_base_url.clone();
    let base_url = normalize_base_url(
        base_url_override
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or(tenant_base.as_deref())
            .unwrap_or(state.config.wechat.base_url.as_str()),
    );
    let endpoint = format!("{base_url}/ilink/bot/get_bot_qrcode?bot_type=3");
    info!(
        tenant_id = %tenant.tenant_id,
        endpoint = %endpoint,
        "调用微信登录二维码接口"
    );

    let body = parse_api_json(
        state
            .http_client
            .get(&endpoint)
            .send()
            .await
            .map_err(|err| ServiceError::Upstream(format!("获取二维码失败: {err}")))?,
        "get_bot_qrcode",
    )
    .await?;
    ensure_success(&body, "get_bot_qrcode")?;
    info!(
        tenant_id = %tenant.tenant_id,
        endpoint = %endpoint,
        ret = body
            .get("ret")
            .and_then(|value| value.as_i64())
            .unwrap_or_default(),
        qrcode = body
            .get("qrcode")
            .and_then(|value| value.as_str())
            .map(mask_identifier)
            .unwrap_or_else(|| "(missing)".to_string()),
        "微信登录二维码接口返回成功"
    );

    let response: LoginQrResponse = serde_json::from_value(body)
        .map_err(|err| ServiceError::Upstream(format!("解析二维码响应失败: {err}")))?;
    info!(
        tenant_id = %tenant.tenant_id,
        qrcode = %mask_identifier(&response.qrcode),
        qrcode_img_content_format = classify_login_qr_content(&response.qrcode_img_content),
        qrcode_img_content_len = response.qrcode_img_content.len(),
        qrcode_img_content_preview = %preview_login_qr_content(&response.qrcode_img_content),
        "微信登录二维码内容格式"
    );

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::{
        MediaKind, MediaPayload, UploadedMedia, build_media_item, classify_login_qr_content,
        infer_media_kind_from_content_type, infer_media_kind_from_name,
        normalize_login_qr_image_data_url, preview_login_qr_content,
    };
    use serde_json::json;

    #[test]
    fn keeps_existing_data_url() {
        let value = "data:image/png;base64,abc123";
        let normalized = normalize_login_qr_image_data_url(value).expect("normalize data url");
        assert_eq!(normalized, value);
    }

    #[test]
    fn wraps_plain_base64_as_png_data_url() {
        let normalized =
            normalize_login_qr_image_data_url("YWJjZA==").expect("normalize base64 payload");
        assert_eq!(normalized, "data:image/png;base64,YWJjZA==");
    }

    #[test]
    fn generates_png_data_url_from_login_page_url() {
        let normalized = normalize_login_qr_image_data_url(
            "https://liteapp.weixin.qq.com/q/7GiQu1?qrcode=test&bot_type=3",
        )
        .expect("normalize login url");
        assert!(normalized.starts_with("data:image/png;base64,"));
        assert!(normalized.len() > "data:image/png;base64,".len());
    }

    #[test]
    fn classifies_login_qr_content_types() {
        assert_eq!(
            classify_login_qr_content("data:image/png;base64,abc123"),
            "data_url"
        );
        assert_eq!(classify_login_qr_content("YWJjZA=="), "base64");
        assert_eq!(
            classify_login_qr_content("https://liteapp.weixin.qq.com/q/test"),
            "url"
        );
        assert_eq!(classify_login_qr_content(""), "empty");
    }

    #[test]
    fn previews_non_url_qr_content_without_full_payload() {
        assert_eq!(
            preview_login_qr_content("data:image/png;base64,abcdefghijklmnopqrstuvwxyz"),
            "data:image/png;base64,ab..."
        );
    }

    #[test]
    fn infers_audio_media_kind_from_content_type_and_name() {
        assert_eq!(
            infer_media_kind_from_content_type(Some("audio/wav")),
            Some(MediaKind::Audio)
        );
        assert_eq!(infer_media_kind_from_name("voice.amr"), MediaKind::Audio);
    }

    #[test]
    fn builds_voice_item_for_audio_payload() {
        let payload = MediaPayload {
            bytes: vec![1, 2, 3],
            file_name: "voice.amr".to_string(),
            media_kind: MediaKind::Audio,
        };
        let uploaded = UploadedMedia {
            download_encrypted_query_param: "voice-token".to_string(),
            aes_key_hex: "00112233445566778899aabbccddeeff".to_string(),
            plaintext_size: 3,
            ciphertext_size: 16,
        };

        assert_eq!(
            build_media_item(&payload, &uploaded),
            json!({
                "type": 3,
                "voice_item": {
                    "media": {
                        "encrypt_query_param": "voice-token",
                        "aes_key": "ABEiM0RVZneImaq7zN3u/w==",
                        "encrypt_type": 1
                    }
                }
            })
        );
    }
}

pub async fn poll_login_status(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    qrcode: &str,
    base_url_override: Option<&str>,
) -> Result<LoginStatusResponse, ServiceError> {
    if qrcode.trim().is_empty() {
        return Err(ServiceError::BadRequest("qrcode 不能为空".to_string()));
    }

    let tenant_base = tenant.credential.read().await.api_base_url.clone();
    let base_url = normalize_base_url(
        base_url_override
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or(tenant_base.as_deref())
            .unwrap_or(state.config.wechat.base_url.as_str()),
    );
    let endpoint = format!("{base_url}/ilink/bot/get_qrcode_status");
    info!(
        tenant_id = %tenant.tenant_id,
        endpoint = %endpoint,
        qrcode = %mask_identifier(qrcode),
        "调用微信扫码状态接口"
    );
    let body = parse_api_json(
        state
            .http_client
            .get(&endpoint)
            .query(&[("qrcode", qrcode)])
            .send()
            .await
            .map_err(|err| ServiceError::Upstream(format!("轮询扫码状态失败: {err}")))?,
        "get_qrcode_status",
    )
    .await?;
    info!(
        tenant_id = %tenant.tenant_id,
        endpoint = %endpoint,
        qrcode = %mask_identifier(qrcode),
        status = body
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown"),
        ilink_bot_id = body.get("ilink_bot_id").and_then(|value| value.as_str()),
        ilink_user_id = body
            .get("ilink_user_id")
            .and_then(|value| value.as_str()),
        "微信扫码状态接口返回"
    );

    serde_json::from_value(body)
        .map_err(|err| ServiceError::Upstream(format!("解析扫码状态失败: {err}")))
}

pub async fn get_updates(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    sync_buf: Option<&str>,
) -> Result<GetUpdatesResponse, ServiceError> {
    let credential = tenant.credential.read().await.clone();
    ensure_tenant_active(&credential)?;

    let token = credential
        .bot_token
        .as_deref()
        .ok_or_else(|| ServiceError::BadRequest("租户缺少 bot_token".to_string()))?;
    let base_url = normalize_base_url(
        credential
            .api_base_url
            .as_deref()
            .unwrap_or(state.config.wechat.base_url.as_str()),
    );
    let endpoint = format!("{base_url}/ilink/bot/getupdates");

    let body_str = json!({
        "get_updates_buf": sync_buf.unwrap_or_default(),
        "base_info": {
            "channel_version": "0.1.0"
        }
    });

    let mut request = state.http_client.post(&endpoint).json(&body_str);
    for (key, value) in build_headers(token) {
        request = request.header(key, value);
    }

    let body = parse_api_json(
        request
            .send()
            .await
            .map_err(|err| ServiceError::Upstream(format!("轮询微信消息失败: {err}")))?,
        "getupdates",
    )
    .await?;

    serde_json::from_value(body)
        .map_err(|err| ServiceError::Upstream(format!("解析 getupdates 响应失败: {err}")))
}

pub async fn send_text_message(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    req: SendTextMessageRequest,
) -> Result<Value, ServiceError> {
    let user_id = req
        .to_user_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ServiceError::BadRequest("toUserId/to_user_id 不能为空".to_string()))?;
    send_text_to_user(
        state,
        tenant,
        user_id,
        &req.text,
        req.context_token.as_deref(),
    )
    .await
}

pub async fn send_media_message(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    req: SendMediaMessageRequest,
) -> Result<Value, ServiceError> {
    let user_id = req
        .to_user_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ServiceError::BadRequest("toUserId/to_user_id 不能为空".to_string()))?;
    send_media_resource_to_user(
        state,
        tenant,
        user_id,
        normalize_optional_text(req.text),
        req.media_url.as_deref(),
        req.media_path.as_deref(),
        req.context_token.as_deref(),
        req.media_type.as_deref(),
        req.file_name.as_deref(),
    )
    .await
}

pub async fn send_text_to_user(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    user_id: &str,
    text: &str,
    context_token: Option<&str>,
) -> Result<Value, ServiceError> {
    let credential = tenant.credential.read().await.clone();
    ensure_tenant_active(&credential)?;
    if text.trim().is_empty() {
        return Err(ServiceError::BadRequest("text 不能为空".to_string()));
    }

    let token = credential
        .bot_token
        .as_deref()
        .ok_or_else(|| ServiceError::BadRequest("租户缺少 bot_token".to_string()))?;
    let base_url = normalize_base_url(
        credential
            .api_base_url
            .as_deref()
            .unwrap_or(state.config.wechat.base_url.as_str()),
    );
    let context_token = resolve_context_token(tenant, user_id, context_token).await?;
    let body = build_sendmessage_body(
        user_id,
        &context_token,
        vec![json!({
            "type": 1,
            "text_item": {
                "text": text
            }
        })],
    );
    send_message_body(state, tenant, token, &base_url, user_id, body).await
}

pub async fn send_media_to_user(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    user_id: &str,
    text: Option<String>,
    media_url: String,
    context_token: Option<&str>,
    media_type: Option<String>,
    file_name: Option<String>,
) -> Result<Value, ServiceError> {
    send_media_resource_to_user(
        state,
        tenant,
        user_id,
        text,
        Some(media_url.as_str()),
        None,
        context_token,
        media_type.as_deref(),
        file_name.as_deref(),
    )
    .await
}

async fn send_media_resource_to_user(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    user_id: &str,
    text: Option<String>,
    media_url: Option<&str>,
    media_path: Option<&str>,
    context_token: Option<&str>,
    media_type: Option<&str>,
    file_name: Option<&str>,
) -> Result<Value, ServiceError> {
    if media_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none()
        && media_path
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_none()
    {
        return Err(ServiceError::BadRequest(
            "media_url/media_path 不能同时为空".to_string(),
        ));
    }

    let credential = tenant.credential.read().await.clone();
    ensure_tenant_active(&credential)?;
    let token = credential
        .bot_token
        .as_deref()
        .ok_or_else(|| ServiceError::BadRequest("租户缺少 bot_token".to_string()))?;
    let base_url = normalize_base_url(
        credential
            .api_base_url
            .as_deref()
            .unwrap_or(state.config.wechat.base_url.as_str()),
    );
    let context_token = resolve_context_token(tenant, user_id, context_token).await?;
    let media_payload =
        load_media_payload(state, media_url, media_path, file_name, media_type).await?;
    let uploaded = upload_media(state, token, &base_url, user_id, &media_payload).await?;

    let mut bodies = Vec::new();
    if let Some(text) = text.filter(|value| !value.trim().is_empty()) {
        bodies.push(build_sendmessage_body(
            user_id,
            &context_token,
            vec![json!({
                "type": 1,
                "text_item": {
                    "text": text
                }
            })],
        ));
    }
    bodies.push(build_sendmessage_body(
        user_id,
        &context_token,
        vec![build_media_item(&media_payload, &uploaded)],
    ));

    let mut last = json!({});
    for body in bodies {
        last = send_message_body(state, tenant, token, &base_url, user_id, body).await?;
    }
    Ok(last)
}

pub async fn send_session_text(
    state: &Arc<AppState>,
    tenant: &Arc<TenantContext>,
    session_key: &str,
    text: &str,
    context_token: Option<&str>,
) -> Result<Value, ServiceError> {
    let user_id = session_key_receiver(session_key)?;
    send_text_to_user(state, tenant, &user_id, text, context_token).await
}

pub fn session_key_receiver(session_key: &str) -> Result<String, ServiceError> {
    let trimmed = session_key.trim();
    let mut parts = trimmed.splitn(3, ':');
    let channel = parts.next().unwrap_or_default();
    let scope = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default().trim();

    if !channel.eq_ignore_ascii_case("wechat") {
        return Err(ServiceError::BadRequest(format!(
            "不支持的 sessionKey channel: {channel}"
        )));
    }
    if target.is_empty() {
        return Err(ServiceError::BadRequest(
            "sessionKey 缺少目标标识".to_string(),
        ));
    }

    match scope {
        "dm" | "user" => Ok(target.to_string()),
        _ => Err(ServiceError::BadRequest(format!(
            "不支持的 sessionKey scope: {scope}"
        ))),
    }
}
