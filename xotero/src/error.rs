use axum::{http::StatusCode, response::IntoResponse, response::Response};

use crate::s3;

pub(crate) fn server_error(context: &str, error: sqlx::Error) -> Response {
    tracing::error!(%error, context, "database error");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

pub(crate) fn s3_error(context: &str, error: s3::S3Error) -> Response {
    tracing::error!(%error, context, "object storage error");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}
