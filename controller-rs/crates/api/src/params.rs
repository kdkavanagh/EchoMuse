//! Path and query parameters, with failures in the API's error shape.

use axum::{
    extract::{FromRequestParts, Path, Query},
    http::request::Parts,
};
use echomuse_core::types::DeviceId;
use serde::de::DeserializeOwned;

use crate::error::ApiError;

/// The `{id}` of a `/api/devices/{id}…` path.
pub(crate) struct DevicePath(pub(crate) DeviceId);

impl<S: Send + Sync> FromRequestParts<S> for DevicePath {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let Path(id) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(|_| ApiError::bad_request("The device id in the path is not readable"))?;
        if id.is_empty() {
            return Err(ApiError::not_found());
        }
        Ok(Self(DeviceId(id)))
    }
}

/// One text path segment.
pub(crate) struct TextPath(pub(crate) String);

impl<S: Send + Sync> FromRequestParts<S> for TextPath {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let Path(text) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(|_| ApiError::bad_request("The path is not readable"))?;
        Ok(Self(text))
    }
}

/// The query string as `T`.
pub(crate) struct QueryParams<T>(pub(crate) T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequestParts<S> for QueryParams<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let Query(value) = Query::<T>::from_request_parts(parts, state)
            .await
            .map_err(|_| ApiError::bad_request("A query parameter is missing or not readable"))?;
        Ok(Self(value))
    }
}
