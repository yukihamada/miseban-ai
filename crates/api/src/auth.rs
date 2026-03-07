use async_trait::async_trait;
use axum::{
    extract::{FromRef, FromRequestParts},
    http::{header, request::Parts, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// JWT secret wrapper (stored in AppState)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct JwtSecret(pub String);

// ---------------------------------------------------------------------------
// JWT claims
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
struct Claims {
    sub: String,
    exp: usize,
}

// ---------------------------------------------------------------------------
// JWT token generation
// ---------------------------------------------------------------------------

/// Issue a JWT token for a given user ID. Expires in 7 days.
pub fn issue_token(user_id: &Uuid, secret: &str) -> Result<String, String> {
    let expiration = chrono::Utc::now()
        .checked_add_signed(chrono::Duration::days(7))
        .expect("valid timestamp")
        .timestamp() as usize;

    let claims = Claims {
        sub: user_id.to_string(),
        exp: expiration,
    };

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| e.to_string())
}

/// Hash a password using bcrypt.
pub fn hash_password(password: &str) -> Result<String, String> {
    bcrypt::hash(password, bcrypt::DEFAULT_COST).map_err(|e| e.to_string())
}

/// Verify a password against a bcrypt hash.
pub fn verify_password(password: &str, hash: &str) -> bool {
    bcrypt::verify(password, hash).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// AuthUser extractor
// ---------------------------------------------------------------------------

/// Extractor that validates a Bearer JWT and yields the authenticated user's UUID.
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
            AuthError::InvalidToken(msg) => {
                (StatusCode::UNAUTHORIZED, format!("Invalid token: {msg}"))
            }
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
    PgPool: FromRef<S>,
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

        // Try JWT first.
        let mut validation = Validation::new(jsonwebtoken::Algorithm::HS256);
        validation.validate_aud = false;

        if let Ok(token_data) = decode::<Claims>(
            token,
            &DecodingKey::from_secret(secret.0.as_bytes()),
            &validation,
        ) {
            let user_id = Uuid::parse_str(&token_data.claims.sub)
                .map_err(|e| AuthError::InvalidSubject(e.to_string()))?;
            return Ok(AuthUser(user_id));
        }

        // JWT failed — try as raw API token.
        let pool = PgPool::from_ref(state);
        let store_id = crate::db::validate_api_token(&pool, token)
            .await
            .ok_or_else(|| AuthError::InvalidToken("InvalidToken".to_string()))?;

        // Get the store's owner_id to return as user_id.
        let store = crate::db::get_store_by_owner_or_id(&pool, &store_id)
            .await
            .ok_or_else(|| AuthError::InvalidToken("store not found".to_string()))?;

        Ok(AuthUser(store.owner_id))
    }
}
