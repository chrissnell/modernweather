//! One error body for the whole management API, and the four things that can go wrong.
//!
//! **Field paths, not a banner.** W5 §7.2 makes server-side validation return
//! `{"field": "...", "message": "..."}` so the error lands on the field the operator typed
//! into rather than in a toast they then cannot act on, and §8.5 makes the `422` body's
//! paths map to the same inline slots a client-side rule would have used. That is the whole
//! reason this is a typed body rather than a string: a `422` whose shape the UI has to guess
//! at degrades into a banner, which is what it replaces.
//!
//! **This surface is not the frozen one.** `mw_api_legacy::reply` reproduces four distinct
//! error conventions byte for byte because the Go tree has four and they are frozen (W1,
//! **D-37**). Nothing here is frozen: the management API is new, versioned at `/api/v1`
//! (**D-49**), and has exactly one error convention.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

/// One problem with one field of a submitted form.
///
/// `field` is a **path** into the request body -- `stations[0].hostname`, `interval_seconds`
/// -- because that is what lets the client put the message beside the input. `message` is in
/// the operator's words and says what to type (§0.1): "Enter a hostname or IP address.", not
/// "invalid hostname".
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct FieldError {
    /// Where in the submitted body the problem is.
    #[schema(example = "stations[0].hostname")]
    pub field: String,
    /// What is wrong, as a sentence that says what to do.
    #[schema(example = "Enter a hostname or IP address.")]
    pub message: String,
}

impl FieldError {
    /// A field error, from anything that renders as a string.
    pub fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

/// The management API's error body.
///
/// `summary` is the one plain line a banner shows when there is no field to attach to;
/// `detail` is the technical half, which W5 §0.4 puts behind a **Details** disclosure rather
/// than on the screen. Both are present on every error so the client never has to decide
/// which one it got.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ErrorBody {
    /// One plain sentence, for a banner.
    pub summary: String,
    /// The technical cause chain, for the disclosure. Empty when there is nothing more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Per-field problems, when the request's *values* are what is wrong.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<FieldError>,
}

/// Why a management request could not be carried out.
///
/// Four variants, each with its own status, because each one has a different audience: the
/// operator's values, a name they used that is not there, a configuration the plan refuses,
/// and the server's own problem.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// The submitted values are wrong, and each problem names its field. `422`.
    ///
    /// **`plan()` errors arrive here too** (§7.2): `PlanError::RouteConflict` is rendered on
    /// the field that caused it -- "Another website already uses `weather.example.com`." --
    /// which is only possible because `plan()` is pure and runs before anything stops.
    #[error("the submitted configuration was refused")]
    Invalid {
        /// One plain line for the banner.
        summary: String,
        /// The per-field problems.
        errors: Vec<FieldError>,
    },

    /// A path named something that is not configured. `404`.
    #[error("there is no {what} called `{id}`")]
    NotFound {
        /// What kind of thing.
        what: &'static str,
        /// What was asked for.
        id: String,
    },

    /// The request was understood and cannot be done in the current state. `409`.
    ///
    /// Distinct from [`ApiError::Invalid`] on purpose: nothing the operator types differently
    /// fixes it. Reverting past the one step the server keeps, and running a component that
    /// is not live, are both this.
    #[error("{summary}")]
    Conflict {
        /// One plain line.
        summary: String,
        /// The technical half.
        detail: Option<String>,
    },

    /// The server could not do its part. `500`.
    #[error("{summary}")]
    Internal {
        /// One plain line.
        summary: String,
        /// The cause chain, rendered at the point it was caught (**D-34**).
        detail: Option<String>,
    },
}

impl ApiError {
    /// A validation refusal with one field error, which is the common case.
    pub fn field(
        summary: impl Into<String>,
        field: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        ApiError::Invalid {
            summary: summary.into(),
            errors: vec![FieldError::new(field, message)],
        }
    }

    /// The status this error answers with.
    pub const fn status(&self) -> StatusCode {
        match self {
            // 422 rather than 400: the body parsed and its *values* are the problem, which
            // is the distinction a client needs to decide between "retry with different
            // input" and "this build and that one disagree about the schema".
            ApiError::Invalid { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::NotFound { .. } => StatusCode::NOT_FOUND,
            ApiError::Conflict { .. } => StatusCode::CONFLICT,
            ApiError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The body, without the status.
    pub fn body(&self) -> ErrorBody {
        match self {
            ApiError::Invalid { summary, errors } => ErrorBody {
                summary: summary.clone(),
                detail: None,
                errors: errors.clone(),
            },
            ApiError::NotFound { .. } => ErrorBody {
                summary: self.to_string(),
                detail: None,
                errors: Vec::new(),
            },
            ApiError::Conflict { summary, detail } | ApiError::Internal { summary, detail } => {
                ErrorBody {
                    summary: summary.clone(),
                    detail: detail.clone(),
                    errors: Vec::new(),
                }
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status(), Json(self.body())).into_response()
    }
}

/// `axum::Json`, with its rejection in **this surface's** error shape.
///
/// §8.5: "Server-side errors use the same slots. The `422` body's `field` paths map to the
/// same inline positions, so an error the client did not predict looks identical to one it
/// did." A bare `Json<T>` answers a malformed body with `axum`'s plain-text `400`, which is
/// the one response on this surface a client could not render beside the input that caused
/// it — and a body whose *shape* is wrong is exactly the unpredicted case that rule is
/// about.
///
/// `serde` already names the field: *"protocol_secret: invalid type: map, expected a
/// string"*. [`Body::field_of`] takes that name and the message follows it, so the client
/// puts it where it would have put its own.
#[derive(Debug, Clone, Copy, Default)]
pub struct Body<T>(pub T);

impl<T, S> axum::extract::FromRequest<S> for Body<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Body(value)),
            Err(rejection) => {
                let message = rejection.body_text();
                Err(ApiError::Invalid {
                    summary: "That is not a request this daemon can read.".to_string(),
                    errors: vec![FieldError::new(Body::<T>::field_of(&message), message)],
                })
            }
        }
    }
}

impl<T> Body<T> {
    /// The field `serde` named, or the whole form when it named none.
    ///
    /// The message is `"<axum's own prefix>: <path>: <what is wrong> at line L column C"`
    /// for a field error, and has no path at all for a body that is not an object. Both the
    /// prefix and the description contain `": "`, so the rule is **the last colon-separated
    /// segment that looks like a path and is not the description itself** — `serde` writes
    /// `stations[0].station_key`, and anything with a space in it is prose.
    ///
    /// Not load-bearing: an unrecognised shape lands on the form, which is where an error
    /// with no field belongs. Guessing a field would be worse than naming none, because the
    /// operator would edit the wrong input and the message would not change.
    fn field_of(message: &str) -> String {
        let segments: Vec<&str> = message.split(": ").collect();
        segments
            .iter()
            .take(segments.len().saturating_sub(1))
            .rfind(|segment| {
                !segment.is_empty()
                    && segment
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "_.[]".contains(c))
            })
            .map_or_else(String::new, |segment| (*segment).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `serde`'s own field path reaches the `field` slot, and a message that is not a path
    /// lands on the form rather than on a guess.
    #[test]
    fn a_body_shape_error_names_the_field_serde_named() {
        assert_eq!(
            Body::<()>::field_of("protocol_secret: invalid type: map, expected a string"),
            "protocol_secret"
        );
        // With `axum`'s own prefix in front, which is how it actually arrives.
        assert_eq!(
            Body::<()>::field_of(
                "Failed to deserialize the JSON body into the target type: \
                 protocol_secret: invalid type: map, expected a string at line 1 column 89"
            ),
            "protocol_secret"
        );
        assert_eq!(
            Body::<()>::field_of("stations[0].station_key: invalid type: null"),
            "stations[0].station_key"
        );
        assert_eq!(Body::<()>::field_of("EOF while parsing a value"), "");
        assert_eq!(
            Body::<()>::field_of("Failed to parse the request body as JSON: expected value"),
            ""
        );
    }

    /// The one thing a client branches on is the status, so each variant has to keep its
    /// own. A shared `400` would make "your value is wrong" and "the server is broken"
    /// indistinguishable, which is the state the Go management API is in.
    #[test]
    fn each_refusal_keeps_its_own_status() {
        assert_eq!(
            ApiError::field("x", "f", "m").status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            ApiError::NotFound {
                what: "weather station",
                id: "backyard".into()
            }
            .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            ApiError::Conflict {
                summary: "x".into(),
                detail: None
            }
            .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            ApiError::Internal {
                summary: "x".into(),
                detail: None
            }
            .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    /// A `404` carries the name that was asked for, because "there is no weather station
    /// called `backyrad`" is the whole diagnosis of the common cause.
    #[test]
    fn a_missing_thing_is_named_in_its_own_message() {
        let body = ApiError::NotFound {
            what: "weather station",
            id: "backyrad".into(),
        }
        .body();
        assert!(body.summary.contains("backyrad"), "{}", body.summary);
        assert!(body.errors.is_empty());
    }

    /// Field errors survive into the body, since the whole point of the `422` is that the
    /// client can put each message beside the input that caused it (§8.5).
    #[test]
    fn a_validation_refusal_carries_its_field_paths() {
        let e = ApiError::Invalid {
            summary: "Check the highlighted fields.".into(),
            errors: vec![
                FieldError::new("hostname", "Enter a hostname or IP address."),
                FieldError::new("latitude", "Enter a latitude between -90 and 90."),
            ],
        };
        let body = e.body();
        assert_eq!(body.errors.len(), 2);
        assert_eq!(body.errors[0].field, "hostname");
        assert!(body.detail.is_none());
    }
}
