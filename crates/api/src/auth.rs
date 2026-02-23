use async_trait::async_trait;
use axum::{
    extract::{FromRef, FromRequestParts},
    http::{header, request::Parts, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use jsonwebtoken::{decode, DecodingKey, Validation};
use serde::Deserialize;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// JWT secret wrapper (stored in AppState)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct JwtSecret(pub String);

// ---------------------------------------------------------------------------
// JWT claims
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Claims {
    sub: String,
    // Supabase JWTs also contain role, aud, exp, etc. We only need `sub`.
}

// ---------------------------------------------------------------------------
// AuthUser extractor
// ---------------------------------------------------------------------------

/// Extractor that validates a Bearer JWT and yields the authenticated user's UUID.
///
/// Usage in handlers:
/// ```ignore
/// async fn my_handler(AuthUser(user_id): AuthUser) -> impl IntoResponse { ... }
/// ```
#[derive(Debug, Clone)]
pub struct AuthUser(pub Uuid);

/// Error type returned when authentication fails.
#[derive(Debug)]
pub enum AuthError {
    MissingHeader,
    InvalidToken(String),
    InvalidSubject(String),
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AuthError::MissingHeader => (
                StatusCode::UNAUTHORIZED,
                "Missing Authorization header".to_string(),
            ),
            AuthError::InvalidToken(msg) => (
                StatusCode::UNAUTHORIZED,
                format!("Invalid token: {msg}"),
            ),
            AuthError::InvalidSubject(msg) => (
                StatusCode::UNAUTHORIZED,
                format!("Invalid subject claim: {msg}"),
            ),
        };

        let body = serde_json::json!({ "error": message });
        (status, Json(body)).into_response()
    }
}

#[async_trait]
impl<S> FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
    JwtSecret: FromRef<S>,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let secret = JwtSecret::from_ref(state);

        // Extract Bearer token from Authorization header.
        let auth_header = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or(AuthError::MissingHeader)?;

        let token = auth_header
            .strip_prefix("Bearer ")
            .ok_or(AuthError::MissingHeader)?;

        // Decode and validate JWT.
        let mut validation = Validation::new(jsonwebtoken::Algorithm::HS256);
        // Supabase JWTs use "authenticated" as audience; accept any for flexibility.
        validation.validate_aud = false;

        let token_data = decode::<Claims>(
            token,
            &DecodingKey::from_secret(secret.0.as_bytes()),
            &validation,
        )
        .map_err(|e| AuthError::InvalidToken(e.to_string()))?;

        // Parse the `sub` claim as a UUID.
        let user_id = Uuid::parse_str(&token_data.claims.sub)
            .map_err(|e| AuthError::InvalidSubject(e.to_string()))?;

        Ok(AuthUser(user_id))
    }
}
