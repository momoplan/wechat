use actix_web::{HttpResponse, ResponseError, http::StatusCode};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("资源不存在: {0}")]
    NotFound(String),
    #[error("请求参数错误: {0}")]
    BadRequest(String),
    #[error("未授权: {0}")]
    Unauthorized(String),
    #[error("上游微信接口错误: {0}")]
    Upstream(String),
    #[error("内部错误: {0}")]
    Internal(String),
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

impl ResponseError for ServiceError {
    fn status_code(&self) -> StatusCode {
        match self {
            ServiceError::NotFound(_) => StatusCode::NOT_FOUND,
            ServiceError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ServiceError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            ServiceError::Upstream(_) => StatusCode::BAD_GATEWAY,
            ServiceError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn error_response(&self) -> HttpResponse {
        HttpResponse::build(self.status_code()).json(ErrorResponse {
            error: self.to_string(),
        })
    }
}

impl From<anyhow::Error> for ServiceError {
    fn from(value: anyhow::Error) -> Self {
        ServiceError::Internal(value.to_string())
    }
}
