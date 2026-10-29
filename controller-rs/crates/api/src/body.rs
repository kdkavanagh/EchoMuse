//! JSON request bodies, with failures in the API's error shape.

use axum::{
    Json,
    extract::{FromRequest, Request, rejection::JsonRejection},
    http::StatusCode,
};
use serde::de::DeserializeOwned;

use crate::error::{ApiError, ErrorCode};

/// A JSON body of type `T`. The request must say `Content-Type: application/json`, so a
/// page on another site cannot send one without the browser asking the controller
/// first. Failures name no field values, since a body may hold a password.
pub(crate) struct JsonBody<T>(pub(crate) T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for JsonBody<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, ApiError> {
        match Json::<T>::from_request(request, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(JsonRejection::MissingJsonContentType(_)) => Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                ErrorCode::UnsupportedMediaType,
                "The request body must be JSON (Content-Type: application/json)",
            )),
            Err(JsonRejection::JsonDataError(_)) => Err(ApiError::bad_request(
                "The request body is missing a field or has one of the wrong type",
            )),
            Err(JsonRejection::JsonSyntaxError(_)) => {
                Err(ApiError::bad_request("The request body is not valid JSON"))
            }
            Err(rejection) => Err(ApiError::new(
                rejection.status(),
                ErrorCode::BadRequest,
                "The request body could not be read",
            )),
        }
    }
}
