mod alerts;
mod auth;
mod billing;
mod csv_export;
mod db;
mod line;
mod plan_guard;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{FromRef, Multipart, Path, Query, State},
    http::{header, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post},
    Json, Router,
};
use chrono::{NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use shared::{
    AgeDistribution, AgeGroup, AnalysisResult, DailyReport, DemographicsSummary, FrameData,
    GenderDistribution, GenderEstimate,
};
use sqlx::PgPool;
use tower_http::cors::CorsLayer;
use tracing::{error, info, warn};
use uuid::Uuid;

use auth::{AuthUser, JwtSecret};
use rand::Rng;

// ---------------------------------------------------------------------------
// AppState
// ---------------------------------------------------------------------------

/// Newtype wrapper so `Option<LineClient>` can derive `FromRef`.
#[derive(Clone)]
struct OptionalLineClient(Option<line::LineClient>);

/// Newtype wrapper so `Option<StripeClient>` can derive `FromRef`.
#[derive(Clone)]
struct OptionalStripeClient(Option<billing::StripeClient>);

#[derive(Clone, FromRef)]
struct AppState {
    pool: PgPool,
    jwt_secret: JwtSecret,
    line_client: OptionalLineClient,
    stripe_client: OptionalStripeClient,
    rate_limiter: plan_guard::RateLimiter,
    /// Per-camera IoU trackers (camera_id → Tracker).
    trackers: Arc<tokio::sync::Mutex<HashMap<String, ai::Tracker>>>,
    /// Optional Gemini API key for age/gender demographics.
    gemini_key: Option<String>,
}

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

#[derive(Debug)]
#[allow(dead_code)]
enum ApiError {
    /// Database query failed.
    Database(String),
    /// The requested resource was not found.
    NotFound(String),
    /// The authenticated user does not own the requested resource.
    Forbidden(String),
    /// Something unexpected happened.
    Internal(String),
    /// Authentication failed (invalid credentials, missing token, etc.).
    Unauthorized(String),
    /// Bad request (validation error, malformed input, etc.).
    BadRequest(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            ApiError::Database(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            ApiError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg.clone()),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
            ApiError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg.clone()),
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
        };

        let body = serde_json::json!({ "error": message });
        (status, Json(body)).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        error!("Database error: {e}");
        ApiError::Database("Database error".to_string())
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// POST /api/v1/frames
///
/// Receives a frame from an agent, runs AI analysis, persists the result,
/// and returns the analysis.
async fn receive_frame(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Json(frame): Json<FrameData>,
) -> Result<(StatusCode, Json<AnalysisResult>), ApiError> {
    // Input validation.
    if frame.camera_id.is_empty() || frame.camera_id.len() > 128 {
        return Err(ApiError::Forbidden("Invalid camera_id".to_string()));
    }
    // Max 10MB JPEG payload.
    const MAX_JPEG_SIZE: usize = 10 * 1024 * 1024;
    if frame.jpeg_bytes.len() > MAX_JPEG_SIZE {
        return Err(ApiError::Forbidden(
            "Frame too large (max 10MB)".to_string(),
        ));
    }
    if frame.jpeg_bytes.is_empty() {
        return Err(ApiError::Forbidden("Empty frame data".to_string()));
    }

    info!(
        camera_id = %frame.camera_id,
        timestamp = %frame.timestamp,
        user_id = %user_id,
        bytes = frame.jpeg_bytes.len(),
        "Frame received"
    );

    // Plan enforcement: check camera limit and rate limit.
    if let Some(store) = db::get_store_by_owner(&state.pool, &user_id).await {
        // Rate limit check (hard enforcement).
        if !state.rate_limiter.check(&store.id, &store.plan_tier).await {
            warn!(
                store_id = %store.id,
                plan_tier = %store.plan_tier,
                "Frame submission rate limited"
            );
            return Err(ApiError::Forbidden(
                "Rate limit exceeded. Please wait before submitting another frame.".to_string(),
            ));
        }

        // Camera limit check (soft enforcement).
        match plan_guard::can_add_camera(&state.pool, &store.id).await {
            Ok(false) => {
                warn!(
                    store_id = %store.id,
                    plan_tier = %store.plan_tier,
                    "Store is at or over camera limit for its plan tier (soft enforcement)"
                );
            }
            Err(e) => {
                warn!(error = %e, "Failed to check plan camera limit (non-fatal)");
            }
            _ => {}
        }
    }

    // Run AI inference: detect people (sync ONNX) + track + demographics.
    let jpeg = frame.jpeg_bytes.clone();
    let detections = tokio::task::spawn_blocking(move || ai::detect_people(&jpeg))
        .await
        .unwrap_or_default();

    // Update per-camera tracker (lock held only for sync update, not during Gemini call).
    let (people_count, zones, tracker_out) = {
        let mut map = state.trackers.lock().await;
        let tracker = map
            .entry(frame.camera_id.clone())
            .or_insert_with(ai::Tracker::new);
        let out = tracker.update(&detections);
        let zones = ai::compute_zones(&detections);
        (out.people_count, zones, out)
    };

    // Demographics via Gemini (optional, async, no lock held).
    let demographics = match &state.gemini_key {
        Some(key) => {
            ai::demographics::estimate(&frame.jpeg_bytes, people_count, key).await
        }
        None => ai::demographics::fallback(people_count),
    };

    let result = shared::AnalysisResult {
        id: uuid::Uuid::new_v4(),
        camera_id: frame.camera_id.clone(),
        timestamp: frame.timestamp,
        people_count,
        demographics,
        zones,
        alerts: vec![],
        avg_dwell_secs: tracker_out.avg_dwell_secs,
        unique_visitors: tracker_out.total_unique,
    };

    // Persist to DB: resolve camera_id to a UUID.
    // The camera_id may be a UUID string or a human-readable name (e.g. "cam-1").
    // If it's not a UUID, look it up by name within the user's store, or auto-register.
    let camera_uuid = match Uuid::parse_str(&result.camera_id) {
        Ok(uuid) => Some(uuid),
        Err(_) => {
            // Not a UUID -- try to resolve by name within the user's store.
            if let Some(store) = db::get_store_by_owner(&state.pool, &user_id).await {
                match db::find_camera_by_name(&state.pool, &store.id, &result.camera_id).await {
                    Some(uuid) => Some(uuid),
                    None => {
                        // Auto-register the camera on first frame.
                        match db::register_camera(&state.pool, &store.id, &result.camera_id).await {
                            Ok(uuid) => {
                                info!(
                                    camera_id = %result.camera_id,
                                    camera_uuid = %uuid,
                                    store_id = %store.id,
                                    "Auto-registered new camera from frame submission"
                                );
                                Some(uuid)
                            }
                            Err(e) => {
                                warn!(
                                    camera_id = %result.camera_id,
                                    error = %e,
                                    "Failed to auto-register camera (non-fatal)"
                                );
                                None
                            }
                        }
                    }
                }
            } else {
                warn!(
                    camera_id = %frame.camera_id,
                    "camera_id is not a UUID and user has no store; skipping DB insert"
                );
                None
            }
        }
    };

    if let Some(ref cam_id) = camera_uuid {
        let demographics_json =
            serde_json::to_value(&result.demographics).unwrap_or(serde_json::Value::Null);
        let zones_json = serde_json::to_value(&result.zones).unwrap_or(serde_json::Value::Null);

        if let Err(e) = db::insert_visitor_count(
            &state.pool,
            cam_id,
            result.people_count as i32,
            demographics_json,
            zones_json,
        )
        .await
        {
            warn!(
                camera_id = %result.camera_id,
                error = %e,
                "Failed to persist visitor count (non-fatal)"
            );
        }
    }

    // Evaluate and persist alerts from AI analysis.
    let pending_alerts = alerts::evaluate_alerts(&result);
    if !pending_alerts.is_empty() {
        if let Some(ref cam_id) = camera_uuid {
            // Look up the store_id for this camera's owner.
            if let Some(store) = db::get_store_by_owner(&state.pool, &user_id).await {
                for pa in &pending_alerts {
                    match alerts::insert_alert(
                        &state.pool,
                        &store.id,
                        cam_id,
                        &pa.alert_type,
                        pa.confidence,
                        &pa.message,
                    )
                    .await
                    {
                        Ok(row) => {
                            info!(
                                alert_id = %row.id,
                                alert_type = %pa.alert_type,
                                "Alert persisted"
                            );

                            // Send LINE notification if configured.
                            if let Some(ref client) = state.line_client.0 {
                                if let Some(line_uid) =
                                    db::get_store_line_user_id(&state.pool, &store.id).await
                                {
                                    let payload = alerts::AlertPayload::from(row);
                                    if let Err(e) =
                                        client.push_alert_message(&line_uid, &payload).await
                                    {
                                        warn!(
                                            error = %e,
                                            "Failed to send LINE alert notification (non-fatal)"
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            warn!(
                                error = %e,
                                alert_type = %pa.alert_type,
                                "Failed to persist alert (non-fatal)"
                            );
                        }
                    }
                }
            }
        }
    }

    Ok((StatusCode::OK, Json(result)))
}

/// Response wrapper for store stats.
#[derive(Serialize)]
struct StoreStats {
    store_id: String,
    current_visitors: i64,
    today_total: i64,
    cameras_online: i64,
    /// Live people count from most recent frame(s) across all cameras.
    live_count: u32,
    /// Average dwell time in seconds (from IoU tracker).
    avg_dwell_secs: f32,
}

/// Response for GET /api/v1/stores/me/stats/weekly.
#[derive(Serialize)]
struct WeeklyStats {
    days: Vec<DailyEntry>,
}

#[derive(Serialize)]
struct DailyEntry {
    date: NaiveDate,
    count: i64,
}

/// Response for GET /api/v1/stores/me/stats/hourly.
#[derive(Serialize)]
struct HourlyStats {
    hours: Vec<HourlyEntry>,
}

#[derive(Serialize)]
struct HourlyEntry {
    hour: i32,
    count: i64,
}

/// GET /api/v1/stores/me/stats
///
/// Returns stats for the authenticated user's store.
async fn get_my_store_stats(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
) -> Result<Json<StoreStats>, ApiError> {
    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("No store found for this user".to_string()))?;

    let (today_total, cameras_online) = db::get_store_stats_db(&state.pool, &store.id).await;

    // Pull live count from in-memory trackers (sum of active tracks across all cameras).
    let (live_count, avg_dwell_secs) = {
        let map = state.trackers.lock().await;
        let total: u32 = map.values().map(|t| t.current_count()).sum();
        let avg: f32 = {
            let counts: Vec<f32> = map.values().map(|t| t.avg_dwell_secs()).filter(|&d| d > 0.0).collect();
            if counts.is_empty() { 0.0 } else { counts.iter().sum::<f32>() / counts.len() as f32 }
        };
        (total, avg)
    };

    Ok(Json(StoreStats {
        store_id: store.id.to_string(),
        current_visitors: today_total,
        today_total,
        cameras_online,
        live_count,
        avg_dwell_secs,
    }))
}

/// GET /api/v1/stores/me/stats/weekly
///
/// Returns daily visitor totals for the past 7 days for the authenticated user's store.
async fn get_weekly_stats(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
) -> Result<Json<WeeklyStats>, ApiError> {
    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("No store found for this user".to_string()))?;

    let rows = db::get_weekly_visitor_counts(&state.pool, &store.id).await;

    let days = rows
        .into_iter()
        .map(|(date, count)| DailyEntry { date, count })
        .collect();

    Ok(Json(WeeklyStats { days }))
}

/// GET /api/v1/stores/me/stats/hourly
///
/// Returns hourly visitor counts for today for the authenticated user's store.
async fn get_hourly_stats(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
) -> Result<Json<HourlyStats>, ApiError> {
    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("No store found for this user".to_string()))?;

    let rows = db::get_hourly_visitor_counts(&state.pool, &store.id).await;

    let hours = rows
        .into_iter()
        .map(|(hour, count)| HourlyEntry { hour, count })
        .collect();

    Ok(Json(HourlyStats { hours }))
}

/// GET /api/v1/stores/:store_id/stats
///
/// Returns stats for a specific store (backwards compat). Requires auth and ownership.
async fn get_store_stats(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Path(store_id): Path<String>,
) -> Result<Json<StoreStats>, ApiError> {
    let store_uuid = Uuid::parse_str(&store_id)
        .map_err(|_| ApiError::NotFound("Invalid store ID".to_string()))?;

    if !db::user_owns_store(&state.pool, &user_id, &store_uuid).await {
        return Err(ApiError::Forbidden("You do not own this store".to_string()));
    }

    let (today_total, cameras_online) = db::get_store_stats_db(&state.pool, &store_uuid).await;

    Ok(Json(StoreStats {
        store_id,
        current_visitors: today_total,
        today_total,
        cameras_online,
        live_count: 0,
        avg_dwell_secs: 0.0,
    }))
}

/// GET /api/v1/stores/me/daily
///
/// Returns the most recent daily report for the authenticated user's store.
async fn get_my_daily_report(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
) -> Result<Json<DailyReport>, ApiError> {
    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("No store found for this user".to_string()))?;

    build_daily_report(&state.pool, &store.id, &store.id.to_string()).await
}

/// GET /api/v1/stores/:store_id/daily
///
/// Returns the daily report for a specific store. Requires auth and ownership.
async fn get_daily_report(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Path(store_id): Path<String>,
) -> Result<Json<DailyReport>, ApiError> {
    let store_uuid = Uuid::parse_str(&store_id)
        .map_err(|_| ApiError::NotFound("Invalid store ID".to_string()))?;

    if !db::user_owns_store(&state.pool, &user_id, &store_uuid).await {
        return Err(ApiError::Forbidden("You do not own this store".to_string()));
    }

    build_daily_report(&state.pool, &store_uuid, &store_id).await
}

/// Shared helper to build a DailyReport from DB data, falling back to mock defaults.
async fn build_daily_report(
    pool: &PgPool,
    store_uuid: &Uuid,
    store_id_str: &str,
) -> Result<Json<DailyReport>, ApiError> {
    if let Some(row) = db::get_daily_report_db(pool, store_uuid).await {
        // Parse demographics_summary from JSONB if present.
        let demographics_summary = row
            .demographics_summary
            .and_then(|v| parse_demographics_summary(&v))
            .unwrap_or_else(default_demographics_summary);

        Ok(Json(DailyReport {
            store_id: store_id_str.to_string(),
            date: row.report_date,
            total_visitors: row.total_visitors as u64,
            peak_hour: row.peak_hour.unwrap_or(0) as u8,
            demographics_summary,
        }))
    } else {
        // No DB data yet -- return sensible defaults.
        let today = Utc::now().date_naive();
        Ok(Json(DailyReport {
            store_id: store_id_str.to_string(),
            date: today,
            total_visitors: 0,
            peak_hour: 0,
            demographics_summary: default_demographics_summary(),
        }))
    }
}

/// Try to parse the demographics_summary JSONB into our shared types.
///
/// Handles two formats:
/// - Array of DemographicEstimate: `[{"age_group":"Adult","gender":"Male","confidence":0.8},...]`
/// - Legacy flat map: `{"male_adult": 10, "female_young_adult": 5, ...}`
fn parse_demographics_summary(value: &serde_json::Value) -> Option<DemographicsSummary> {
    // Array format (from new Gemini-powered inference)
    if let Some(arr) = value.as_array() {
        if arr.is_empty() {
            return None;
        }
        let mut male = 0u32;
        let mut female = 0u32;
        let mut unknown = 0u32;
        let mut child = 0u32;
        let mut teen = 0u32;
        let mut young_adult = 0u32;
        let mut adult = 0u32;
        let mut senior = 0u32;

        for item in arr {
            let gender = item["gender"].as_str().unwrap_or("Unknown");
            match gender {
                "Male" => male += 1,
                "Female" => female += 1,
                _ => unknown += 1,
            }
            let age = item["age_group"].as_str().unwrap_or("Adult");
            match age {
                "Child" => child += 1,
                "Teen" => teen += 1,
                "YoungAdult" => young_adult += 1,
                "Adult" => adult += 1,
                "Senior" => senior += 1,
                _ => adult += 1,
            }
        }
        let g_total = (male + female + unknown) as f32;
        let a_total = (child + teen + young_adult + adult + senior) as f32;
        if g_total == 0.0 {
            return None;
        }
        return Some(DemographicsSummary {
            age_distribution: vec![
                AgeDistribution { age_group: AgeGroup::Child,      percentage: child as f32 / a_total * 100.0 },
                AgeDistribution { age_group: AgeGroup::Teen,       percentage: teen as f32 / a_total * 100.0 },
                AgeDistribution { age_group: AgeGroup::YoungAdult, percentage: young_adult as f32 / a_total * 100.0 },
                AgeDistribution { age_group: AgeGroup::Adult,      percentage: adult as f32 / a_total * 100.0 },
                AgeDistribution { age_group: AgeGroup::Senior,     percentage: senior as f32 / a_total * 100.0 },
            ],
            gender_distribution: vec![
                GenderDistribution { gender: GenderEstimate::Male,    percentage: male as f32 / g_total * 100.0 },
                GenderDistribution { gender: GenderEstimate::Female,  percentage: female as f32 / g_total * 100.0 },
                GenderDistribution { gender: GenderEstimate::Unknown, percentage: unknown as f32 / g_total * 100.0 },
            ],
        });
    }

    // Legacy flat-map format
    let obj = value.as_object()?;
    if obj.is_empty() {
        return None;
    }
    let total: f64 = obj.values().filter_map(|v| v.as_f64()).sum();
    if total <= 0.0 {
        return None;
    }
    let mut male_count = 0.0_f64;
    let mut female_count = 0.0_f64;
    let mut other_count = 0.0_f64;
    for (key, val) in obj {
        let count = val.as_f64().unwrap_or(0.0);
        if key.starts_with("male") {
            male_count += count;
        } else if key.starts_with("female") {
            female_count += count;
        } else {
            other_count += count;
        }
    }
    let gender_total = male_count + female_count + other_count;
    Some(DemographicsSummary {
        age_distribution: default_age_distribution(),
        gender_distribution: if gender_total > 0.0 {
            vec![
                GenderDistribution { gender: GenderEstimate::Male,    percentage: (male_count / gender_total * 100.0) as f32 },
                GenderDistribution { gender: GenderEstimate::Female,  percentage: (female_count / gender_total * 100.0) as f32 },
                GenderDistribution { gender: GenderEstimate::Unknown, percentage: (other_count / gender_total * 100.0) as f32 },
            ]
        } else {
            default_gender_distribution()
        },
    })
}

fn default_demographics_summary() -> DemographicsSummary {
    DemographicsSummary {
        age_distribution: default_age_distribution(),
        gender_distribution: default_gender_distribution(),
    }
}

fn default_age_distribution() -> Vec<AgeDistribution> {
    vec![
        AgeDistribution {
            age_group: AgeGroup::Child,
            percentage: 0.0,
        },
        AgeDistribution {
            age_group: AgeGroup::Teen,
            percentage: 0.0,
        },
        AgeDistribution {
            age_group: AgeGroup::YoungAdult,
            percentage: 0.0,
        },
        AgeDistribution {
            age_group: AgeGroup::Adult,
            percentage: 0.0,
        },
        AgeDistribution {
            age_group: AgeGroup::Senior,
            percentage: 0.0,
        },
    ]
}

fn default_gender_distribution() -> Vec<GenderDistribution> {
    vec![
        GenderDistribution {
            gender: GenderEstimate::Male,
            percentage: 0.0,
        },
        GenderDistribution {
            gender: GenderEstimate::Female,
            percentage: 0.0,
        },
        GenderDistribution {
            gender: GenderEstimate::Unknown,
            percentage: 0.0,
        },
    ]
}

/// GET /api/v1/stores/me/cameras
///
/// Lists cameras for the authenticated user's store.
async fn get_my_cameras(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<db::CameraRow>>, ApiError> {
    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("No store found for this user".to_string()))?;

    let cameras = db::get_cameras(&state.pool, &store.id).await;
    Ok(Json(cameras))
}

/// POST /api/v1/stores/me/cameras
///
/// Creates a new camera entry for the authenticated user's store.
#[derive(Debug, serde::Deserialize)]
struct CreateCameraRequest {
    name: String,
    rtsp_url: Option<String>,
    location: Option<String>,
}

async fn create_my_camera(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateCameraRequest>,
) -> Result<Json<db::CameraRow>, ApiError> {
    if req.name.trim().is_empty() {
        return Err(ApiError::BadRequest("Camera name is required".to_string()));
    }

    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("No store found for this user".to_string()))?;

    // Check camera limit for plan
    if !plan_guard::can_add_camera(&state.pool, &store.id).await.unwrap_or(false) {
        return Err(ApiError::Forbidden("Camera limit reached for your plan".to_string()));
    }

    let camera = db::create_camera(&state.pool, &store.id, &req.name, req.rtsp_url.as_deref(), req.location.as_deref()).await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(Json(camera))
}

/// DELETE /api/v1/stores/me/cameras/:camera_id
async fn delete_my_camera(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    axum::extract::Path(camera_id): axum::extract::Path<uuid::Uuid>,
) -> Result<StatusCode, ApiError> {
    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("No store found".to_string()))?;
    sqlx::query("DELETE FROM cameras WHERE id = $1 AND store_id = $2")
        .bind(camera_id)
        .bind(store.id)
        .execute(&state.pool)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

/// GET /api/v1/agent/config
///
/// Returns the camera list and store info for the agent. Uses the same JWT as the
/// dashboard so no separate auth is needed — the agent receives the token during pairing.
#[derive(Serialize)]
struct AgentConfigResponse {
    store_id: String,
    store_name: String,
    cameras: Vec<db::CameraRow>,
}

async fn agent_config(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
) -> Result<Json<AgentConfigResponse>, ApiError> {
    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("No store found".to_string()))?;
    let cameras = db::get_cameras(&state.pool, &store.id).await;
    Ok(Json(AgentConfigResponse {
        store_id: store.id.to_string(),
        store_name: store.name,
        cameras,
    }))
}

/// POST /api/v1/agent/heartbeat
///
/// Called periodically by the agent to report it is online.
#[derive(Deserialize)]
struct HeartbeatRequest {
    version: Option<String>,
}

async fn agent_heartbeat(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Json(body): Json<HeartbeatRequest>,
) -> Result<StatusCode, ApiError> {
    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("No store found".to_string()))?;
    sqlx::query(
        "UPDATE stores SET agent_last_seen_at = now(), agent_version = $1 WHERE id = $2",
    )
    .bind(body.version.as_deref())
    .bind(store.id)
    .execute(&state.pool)
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Usage / plan guard
// ---------------------------------------------------------------------------

/// Response for GET /api/v1/stores/me/usage.
#[derive(Serialize)]
struct UsageResponse {
    plan_tier: String,
    cameras_used: i64,
    cameras_max: serde_json::Value, // number or "unlimited"
    retention_days: i64,
    features: UsageFeatures,
}

#[derive(Serialize)]
struct UsageFeatures {
    demographics: bool,
    heatmaps: bool,
    alerts: bool,
    line_alerts: bool,
    csv_export: bool,
    api_access: bool,
}

/// GET /api/v1/stores/me/usage
///
/// Returns current plan usage and feature flags for the authenticated user's store.
async fn get_my_usage(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
) -> Result<Json<UsageResponse>, ApiError> {
    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("No store found for this user".to_string()))?;

    let tier = &store.plan_tier;
    let cameras_used = db::count_cameras(&state.pool, &store.id).await;
    let max = plan_guard::max_cameras_for_tier(tier);
    let cameras_max = if max == usize::MAX {
        serde_json::json!("unlimited")
    } else {
        serde_json::json!(max)
    };

    Ok(Json(UsageResponse {
        plan_tier: tier.clone(),
        cameras_used,
        cameras_max,
        retention_days: plan_guard::retention_days_for_tier(tier),
        features: UsageFeatures {
            demographics: plan_guard::tier_has_feature(tier, "demographics"),
            heatmaps: plan_guard::tier_has_feature(tier, "heatmaps"),
            alerts: plan_guard::tier_has_feature(tier, "alerts"),
            line_alerts: plan_guard::tier_has_feature(tier, "line_alerts"),
            csv_export: plan_guard::tier_has_feature(tier, "csv_export"),
            api_access: plan_guard::tier_has_feature(tier, "api_access"),
        },
    }))
}

// ---------------------------------------------------------------------------
// Alert handlers
// ---------------------------------------------------------------------------

/// Query params for GET /api/v1/stores/me/alerts.
#[derive(Debug, Deserialize)]
struct AlertsQuery {
    limit: Option<i64>,
    unread_only: Option<bool>,
}

/// GET /api/v1/stores/me/alerts
///
/// Returns recent alerts for the authenticated user's store.
async fn get_my_alerts(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Query(params): Query<AlertsQuery>,
) -> Result<Json<Vec<alerts::AlertPayload>>, ApiError> {
    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("No store found for this user".to_string()))?;

    let limit = params.limit.unwrap_or(20).min(100);
    let unread_only = params.unread_only.unwrap_or(false);

    let rows = alerts::get_recent_alerts(&state.pool, &store.id, limit, unread_only).await;
    let payloads: Vec<alerts::AlertPayload> = rows.into_iter().map(Into::into).collect();

    Ok(Json(payloads))
}

/// GET /api/v1/stores/me/alerts/count
///
/// Returns the count of unread alerts.
async fn get_my_alert_count(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("No store found for this user".to_string()))?;

    let unread = alerts::get_unread_alert_count(&state.pool, &store.id).await;

    Ok(Json(serde_json::json!({ "unread": unread })))
}

/// PATCH /api/v1/alerts/:alert_id/read
///
/// Mark a single alert as read.
async fn mark_alert_read(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Path(alert_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("No store found for this user".to_string()))?;

    let alert_uuid = Uuid::parse_str(&alert_id)
        .map_err(|_| ApiError::NotFound("Invalid alert ID".to_string()))?;

    let updated = alerts::mark_alert_read(&state.pool, &alert_uuid, &store.id).await?;

    if updated {
        Ok(StatusCode::OK)
    } else {
        Err(ApiError::NotFound(
            "Alert not found or already read".to_string(),
        ))
    }
}

/// POST /api/v1/alerts/read-all
///
/// Mark all alerts as read for the authenticated user's store.
async fn mark_all_alerts_read(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("No store found for this user".to_string()))?;

    let count = alerts::mark_all_alerts_read(&state.pool, &store.id).await?;

    Ok(Json(serde_json::json!({ "marked": count })))
}

// ---------------------------------------------------------------------------
// CSV export handler
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ExportCsvQuery {
    from: Option<chrono::NaiveDate>,
    to: Option<chrono::NaiveDate>,
}

/// GET /api/v1/stores/me/export/csv
async fn export_csv(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Query(params): Query<ExportCsvQuery>,
) -> Result<Response, ApiError> {
    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("No store found for this user".to_string()))?;

    if !plan_guard::tier_has_feature(&store.plan_tier, "csv_export") {
        return Err(ApiError::Forbidden(
            "CSV export requires Pro or Enterprise plan".to_string(),
        ));
    }

    let today = Utc::now().date_naive();
    let to = params.to.unwrap_or(today);
    let from = params
        .from
        .unwrap_or_else(|| to - chrono::Duration::days(30));

    let csv = csv_export::export_visitor_csv(&state.pool, &store.id, from, to).await?;

    let filename = format!("miseban-export-{}-{}.csv", from, to);
    let disposition = format!("attachment; filename=\"{}\"", filename);

    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (header::CONTENT_DISPOSITION, &disposition),
        ],
        csv,
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// Billing handlers
// ---------------------------------------------------------------------------

/// GET /api/v1/pricing
///
/// Public endpoint returning all pricing plans.
async fn get_pricing() -> Json<Vec<billing::PricingPlan>> {
    Json(billing::get_pricing_plans())
}

/// Request body for POST /api/v1/billing/checkout.
#[derive(Debug, Deserialize)]
struct CheckoutRequest {
    /// Plan tier name ("starter", "pro", "enterprise"). Preferred over price_id.
    tier: Option<String>,
    /// Direct Stripe price ID (legacy; use `tier` instead).
    price_id: Option<String>,
    success_url: String,
    cancel_url: String,
}

/// POST /api/v1/billing/checkout
///
/// Creates a Stripe Checkout Session for subscription. Requires authentication.
async fn create_checkout(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Json(body): Json<CheckoutRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let stripe = state
        .stripe_client
        .0
        .as_ref()
        .ok_or_else(|| ApiError::Internal("Stripe is not configured".to_string()))?;

    // Resolve price_id from tier name or use direct price_id.
    let price_id = if let Some(tier) = &body.tier {
        let env_var = match tier.as_str() {
            "starter" => "STRIPE_PRICE_STARTER",
            "pro" => "STRIPE_PRICE_PRO",
            "enterprise" => "STRIPE_PRICE_ENTERPRISE",
            _ => return Err(ApiError::BadRequest(format!("Unknown tier: {tier}"))),
        };
        std::env::var(env_var)
            .map_err(|_| ApiError::Internal(format!("Price ID not configured for tier: {tier}")))?
    } else if let Some(pid) = &body.price_id {
        pid.clone()
    } else {
        return Err(ApiError::BadRequest("Must provide either 'tier' or 'price_id'".to_string()));
    };

    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("No store found for this user".to_string()))?;

    // Use actual user email for Stripe customer.
    let customer_email = db::get_user_by_id(&state.pool, &user_id)
        .await
        .map(|u| u.email)
        .unwrap_or_else(|| format!("user-{}@misebanai.com", user_id));

    let session = stripe
        .create_checkout_session(
            &price_id,
            &customer_email,
            &store.id,
            &body.success_url,
            &body.cancel_url,
        )
        .await
        .map_err(|e| ApiError::Internal(format!("Stripe checkout error: {e}")))?;

    let url = session
        .url
        .ok_or_else(|| ApiError::Internal("Stripe returned no checkout URL".to_string()))?;

    Ok(Json(serde_json::json!({ "url": url })))
}

/// Request body for POST /api/v1/billing/portal.
#[derive(Debug, Deserialize)]
struct PortalRequest {
    return_url: String,
}

/// POST /api/v1/billing/portal
///
/// Creates a Stripe Customer Portal session. Requires the store to have a stripe_customer_id.
async fn create_portal(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Json(body): Json<PortalRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let stripe = state
        .stripe_client
        .0
        .as_ref()
        .ok_or_else(|| ApiError::Internal("Stripe is not configured".to_string()))?;

    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("No store found for this user".to_string()))?;

    let customer_id = billing::get_stripe_customer_id(&state.pool, &store.id)
        .await
        .ok_or_else(|| ApiError::NotFound("No Stripe customer linked to this store".to_string()))?;

    let session = stripe
        .create_portal_session(&customer_id, &body.return_url)
        .await
        .map_err(|e| ApiError::Internal(format!("Stripe portal error: {e}")))?;

    Ok(Json(serde_json::json!({ "url": session.url })))
}

/// GET /api/v1/billing/subscription
///
/// Returns current subscription info for the authenticated user's store.
async fn get_subscription(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
) -> Result<Json<billing::SubscriptionInfo>, ApiError> {
    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("No store found for this user".to_string()))?;

    let info = billing::get_subscription_status(&state.pool, &store.id).await;
    Ok(Json(info))
}

/// POST /api/v1/webhooks/stripe
///
/// Stripe webhook endpoint (public, no JWT auth). Parses raw body and handles events.
/// Verifies webhook signature if STRIPE_WEBHOOK_SECRET is configured.
async fn stripe_webhook(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Result<StatusCode, ApiError> {
    // Verify Stripe webhook signature if secret is configured.
    if let Ok(secret) = std::env::var("STRIPE_WEBHOOK_SECRET") {
        let sig = headers
            .get("stripe-signature")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if let Err(e) = billing::verify_stripe_signature(&body, sig, &secret) {
            warn!(error = %e, "Stripe webhook signature verification failed");
            return Err(ApiError::Forbidden(format!("Invalid signature: {e}")));
        }
    }

    let event: billing::StripeEvent = serde_json::from_str(&body)
        .map_err(|e| ApiError::Internal(format!("Invalid webhook payload: {e}")))?;

    info!(event_type = %event.event_type, "Stripe webhook received");

    match event.event_type.as_str() {
        "checkout.session.completed" => {
            let obj = &event.data.object;
            let customer_id = obj["customer"].as_str().unwrap_or_default();
            let store_id_str = obj["metadata"]["store_id"].as_str().unwrap_or_default();

            if let Ok(store_id) = Uuid::parse_str(store_id_str) {
                let _ = billing::set_stripe_customer_id(&state.pool, &store_id, customer_id).await;
                let tier = billing::determine_tier(obj);
                let _ = billing::update_plan_tier(&state.pool, &store_id, tier).await;
                info!(store_id = %store_id, tier = tier, "Checkout completed, plan set");

                // Send contract confirmed email (fire-and-forget)
                if let Some(store) = db::get_store_by_owner_or_id(&state.pool, &store_id).await {
                    if let Some(user) = db::get_user_by_id(&state.pool, &store.owner_id).await {
                        let email = user.email.clone();
                        let store_name = store.name.clone();
                        let tier_str = tier.to_string();
                        let http = reqwest::Client::new();
                        tokio::spawn(async move {
                            let resend_key = std::env::var("RESEND_API_KEY").ok();
                            if let Some(key) = resend_key {
                                let body = build_subscription_activated_email(&store_name, &tier_str);
                                let _ = http.post("https://api.resend.com/emails")
                                    .header("Authorization", format!("Bearer {key}"))
                                    .json(&serde_json::json!({
                                        "from": "ミセバンAI <noreply@misebanai.com>",
                                        "to": [email],
                                        "subject": format!("【ミセバンAI】{}プランのご契約ありがとうございます", tier_str),
                                        "html": body,
                                    }))
                                    .send().await;
                            }
                        });
                    }
                }
            }
        }
        "customer.subscription.deleted" => {
            let customer_id = event.data.object["customer"].as_str().unwrap_or_default();
            if let Some((store_id, _)) =
                billing::get_store_by_stripe_customer(&state.pool, customer_id).await
            {
                let _ = billing::update_plan_tier(&state.pool, &store_id, "free").await;
                info!(store_id = %store_id, "Subscription cancelled, downgraded to free");

                // Send cancellation email (fire-and-forget)
                if let Some(store) = db::get_store_by_owner_or_id(&state.pool, &store_id).await {
                    if let Some(user) = db::get_user_by_id(&state.pool, &store.owner_id).await {
                        let email = user.email.clone();
                        let store_name = store.name.clone();
                        let http = reqwest::Client::new();
                        tokio::spawn(async move {
                            let resend_key = std::env::var("RESEND_API_KEY").ok();
                            if let Some(key) = resend_key {
                                let body = build_subscription_cancelled_email(&store_name);
                                let _ = http.post("https://api.resend.com/emails")
                                    .header("Authorization", format!("Bearer {key}"))
                                    .json(&serde_json::json!({
                                        "from": "ミセバンAI <noreply@misebanai.com>",
                                        "to": [email],
                                        "subject": "【ミセバンAI】サブスクリプションの解約を受け付けました",
                                        "html": body,
                                    }))
                                    .send().await;
                            }
                        });
                    }
                }
            }
        }
        "invoice.payment_failed" => {
            let customer_id = event.data.object["customer"].as_str().unwrap_or_default();
            warn!(customer_id = %customer_id, "Payment failed");

            // Send payment failed email (fire-and-forget)
            if let Some((store_id, _)) =
                billing::get_store_by_stripe_customer(&state.pool, customer_id).await
            {
                if let Some(store) = db::get_store_by_owner_or_id(&state.pool, &store_id).await {
                    if let Some(user) = db::get_user_by_id(&state.pool, &store.owner_id).await {
                        let email = user.email.clone();
                        let store_name = store.name.clone();
                        let http = reqwest::Client::new();
                        tokio::spawn(async move {
                            let resend_key = std::env::var("RESEND_API_KEY").ok();
                            if let Some(key) = resend_key {
                                let body = build_payment_failed_email(&store_name);
                                let _ = http.post("https://api.resend.com/emails")
                                    .header("Authorization", format!("Bearer {key}"))
                                    .json(&serde_json::json!({
                                        "from": "ミセバンAI <noreply@misebanai.com>",
                                        "to": [email],
                                        "subject": "【ミセバンAI】お支払いに問題が発生しました",
                                        "html": body,
                                    }))
                                    .send().await;
                            }
                        });
                    }
                }
            }
        }
        _ => {
            tracing::debug!(event_type = %event.event_type, "Unhandled Stripe event");
        }
    }

    Ok(StatusCode::OK)
}

/// POST /api/v1/webhooks/line
///
/// LINE webhook endpoint (public, no JWT auth). Receives events from LINE platform.
/// Verifies webhook signature if LINE_CHANNEL_SECRET is configured.
async fn line_webhook(headers: axum::http::HeaderMap, body: String) -> StatusCode {
    // Verify LINE signature if channel secret is configured.
    if let Ok(secret) = std::env::var("LINE_CHANNEL_SECRET") {
        let sig = headers
            .get("x-line-signature")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !line::verify_line_signature(body.as_bytes(), sig, &secret) {
            warn!("LINE webhook signature verification failed");
            return StatusCode::FORBIDDEN;
        }
    }

    let body: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "Invalid LINE webhook payload");
            return StatusCode::BAD_REQUEST;
        }
    };

    if let Some(events) = body.get("events").and_then(|v| v.as_array()) {
        for event in events {
            let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let user_id = event
                .get("source")
                .and_then(|s| s.get("userId"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            match event_type {
                "follow" => {
                    info!(line_user_id = user_id, "LINE follow event received");
                }
                "unfollow" => {
                    info!(line_user_id = user_id, "LINE unfollow event received");
                }
                _ => {
                    info!(
                        event_type,
                        line_user_id = user_id,
                        "LINE webhook event received"
                    );
                }
            }
        }
    }

    StatusCode::OK
}

// ---------------------------------------------------------------------------
// Camera pairing endpoint
// ---------------------------------------------------------------------------

/// Request body for POST /api/v1/pair.
#[derive(Debug, Deserialize)]
struct PairRequest {
    code: String,
}

/// Response for POST /api/v1/pair.
#[derive(Debug, Serialize)]
struct PairResponse {
    token: String,
    store_id: String,
    store_name: String,
}

/// POST /api/v1/pair
///
/// Accepts a 6-digit pairing code from the camera agent setup wizard.
/// Validates the code, returns an API token and store info so the agent
/// can start uploading frames.
async fn handle_pair(
    State(state): State<AppState>,
    Json(body): Json<PairRequest>,
) -> Result<Json<PairResponse>, ApiError> {
    let code = body.code.trim().to_string();

    // Validate code format: exactly 6 digits.
    if code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) {
        return Err(ApiError::BadRequest(
            "Pairing code must be exactly 6 digits".to_string(),
        ));
    }

    // Look up and consume the pairing code.
    let (store_id, token) = db::consume_pairing_code(&state.pool, &code)
        .await
        .ok_or_else(|| {
            ApiError::NotFound(
                "Invalid or expired pairing code. Please generate a new code from the dashboard."
                    .to_string(),
            )
        })?;

    // Fetch the store name.
    let store = db::get_store_by_owner_or_id(&state.pool, &store_id).await;
    let store_name = store
        .map(|s| s.name)
        .unwrap_or_else(|| "My Store".to_string());

    info!(
        store_id = %store_id,
        store_name = %store_name,
        "Camera agent paired successfully"
    );

    Ok(Json(PairResponse {
        token,
        store_id: store_id.to_string(),
        store_name,
    }))
}

/// POST /api/v1/pair/generate
///
/// Generates a new pairing code for the authenticated user's store.
/// The code is valid for 10 minutes.
async fn generate_pairing_code(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("No store found for this user".to_string()))?;

    // Issue a token for the agent to use.
    let token = auth::issue_token(&user_id, &state.jwt_secret.0)
        .map_err(|e| ApiError::Internal(format!("Token error: {e}")))?;

    let code = db::create_pairing_code(&state.pool, &store.id, &token)
        .await
        .map_err(|e| ApiError::Internal(format!("Failed to create pairing code: {e}")))?;

    info!(
        store_id = %store.id,
        code = %code,
        "Pairing code generated"
    );

    Ok(Json(serde_json::json!({
        "code": code,
        "expires_in_seconds": 600,
    })))
}

// ---------------------------------------------------------------------------
// Auth endpoints (passwordless OTP)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct AuthRequest {
    email: String,
    #[serde(default)]
    password: String,
    store_name: Option<String>,
}

#[derive(Deserialize)]
struct SendOtpRequest {
    email: String,
}

#[derive(Deserialize)]
struct VerifyOtpRequest {
    email: String,
    code: String,
}

#[derive(Serialize)]
struct AuthResponse {
    token: String,
    user_id: String,
}

#[derive(Serialize)]
struct OtpSentResponse {
    sent: bool,
}

/// POST /api/v1/auth/send-otp
/// Sends a magic login link to the given email. Creates the user if not yet registered.
#[axum::debug_handler]
async fn send_otp(
    State(state): State<AppState>,
    Json(body): Json<SendOtpRequest>,
) -> Result<Json<OtpSentResponse>, ApiError> {
    let email = body.email.trim().to_lowercase();
    if email.is_empty() || !email.contains('@') {
        return Err(ApiError::BadRequest("Invalid email".to_string()));
    }

    // Auto-create user if not exists (email-only registration)
    if db::find_user_by_email(&state.pool, &email).await.is_none() {
        let placeholder_hash = "passwordless".to_string();
        let user_id = db::create_user(&state.pool, &email, &placeholder_hash)
            .await
            .map_err(|e| ApiError::Internal(format!("DB error: {e}")))?;
        let store_name = format!("{}のお店", email.split('@').next().unwrap_or("user"));
        if let Err(e) = db::create_default_store(&state.pool, &user_id, &store_name).await {
            warn!(error = %e, "Failed to create default store");
        }
        // Welcome email (fire-and-forget)
        {
            let email_c = email.clone();
            let store_c = store_name.clone();
            let resend_key = std::env::var("RESEND_API_KEY").ok();
            if let Some(key) = resend_key {
                let client = reqwest::Client::new();
                tokio::spawn(async move {
                    let body = build_welcome_email(&store_c);
                    let _ = client
                        .post("https://api.resend.com/emails")
                        .header("Authorization", format!("Bearer {key}"))
                        .json(&serde_json::json!({
                            "from": "ミセバンAI <noreply@misebanai.com>",
                            "to": [&email_c],
                            "subject": "【ミセバンAI】アカウント登録が完了しました",
                            "html": body,
                        }))
                        .send()
                        .await;
                });
            }
        }
    }

    // Generate magic token (UUID)
    let token = uuid::Uuid::new_v4().to_string();
    let expires_at = chrono::Utc::now() + chrono::Duration::minutes(15);

    sqlx::query(
        "INSERT INTO otp_codes (email, code, expires_at) VALUES ($1, $2, $3)"
    )
    .bind(&email)
    .bind(&token)
    .bind(expires_at)
    .execute(&state.pool)
    .await
    .map_err(|e| ApiError::Internal(format!("DB error: {e}")))?;

    let magic_url = format!("https://misebanai.com/dashboard/?magic={}", token);

    // Send magic link email
    let resend_key = std::env::var("RESEND_API_KEY").ok();
    if let Some(key) = resend_key {
        let email_c = email.clone();
        let url_c = magic_url.clone();
        let client = reqwest::Client::new();
        tokio::spawn(async move {
            let html = format!(
                r#"<div style="font-family:sans-serif;max-width:480px;margin:0 auto;padding:32px">
                <img src="https://misebanai.com/favicon-32.png" width="32" height="32" style="margin-bottom:16px">
                <h2 style="color:#1e293b;margin-bottom:8px">ミセバンAI にログイン</h2>
                <p style="color:#64748b;margin-bottom:24px">下のボタンをクリックするとログインできます。有効期限は15分です。</p>
                <div style="text-align:center;margin-bottom:24px">
                  <a href="{}" style="display:inline-block;background:#4f46e5;color:#fff;font-weight:700;font-size:1rem;padding:14px 32px;border-radius:100px;text-decoration:none">ログインする</a>
                </div>
                <p style="color:#94a3b8;font-size:0.75rem">ボタンが押せない場合は以下のURLをブラウザに貼り付けてください：<br>{}</p>
                <p style="color:#94a3b8;font-size:0.75rem;margin-top:8px">このメールに心当たりがない場合は無視してください。</p>
                </div>"#,
                url_c, url_c
            );
            let _ = client
                .post("https://api.resend.com/emails")
                .header("Authorization", format!("Bearer {key}"))
                .json(&serde_json::json!({
                    "from": "ミセバンAI <noreply@misebanai.com>",
                    "to": [&email_c],
                    "subject": "ミセバンAI へのログインリンク",
                    "html": html,
                }))
                .send()
                .await;
        });
    } else {
        info!(email = %email, magic_url = %magic_url, "Magic link (no RESEND_API_KEY set)");
    }

    Ok(Json(OtpSentResponse { sent: true }))
}

/// GET /api/v1/auth/magic?token=UUID
/// Verifies a magic link token and returns a JWT.
async fn magic_login(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<AuthResponse>, ApiError> {
    let token = params.get("token")
        .ok_or_else(|| ApiError::BadRequest("Missing token".to_string()))?
        .clone();

    let row: Option<(uuid::Uuid, bool, chrono::DateTime<chrono::Utc>, String)> = sqlx::query_as(
        "SELECT id, used, expires_at, email FROM otp_codes \
         WHERE code = $1 ORDER BY created_at DESC LIMIT 1"
    )
    .bind(&token)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| ApiError::Internal(format!("DB error: {e}")))?;

    let (otp_id, used, expires_at, email) = row
        .ok_or_else(|| ApiError::Unauthorized("Invalid token".to_string()))?;
    if used {
        return Err(ApiError::Unauthorized("Link already used".to_string()));
    }
    if chrono::Utc::now() > expires_at {
        return Err(ApiError::Unauthorized("Link expired".to_string()));
    }

    sqlx::query("UPDATE otp_codes SET used = true WHERE id = $1")
        .bind(otp_id)
        .execute(&state.pool)
        .await
        .map_err(|e| ApiError::Internal(format!("DB error: {e}")))?;

    let user = db::find_user_by_email(&state.pool, &email)
        .await
        .ok_or_else(|| ApiError::Unauthorized("User not found".to_string()))?;

    let jwt = auth::issue_token(&user.id, &state.jwt_secret.0)
        .map_err(|e| ApiError::Internal(format!("Token error: {e}")))?;

    Ok(Json(AuthResponse { token: jwt, user_id: user.id.to_string() }))
}

/// POST /api/v1/auth/verify-otp
/// Verifies the OTP and returns a JWT token.
async fn verify_otp(
    State(state): State<AppState>,
    Json(body): Json<VerifyOtpRequest>,
) -> Result<Json<AuthResponse>, ApiError> {
    let email = body.email.trim().to_lowercase();
    let code = body.code.trim().to_string();

    let row: Option<(uuid::Uuid, bool, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT id, used, expires_at FROM otp_codes \
         WHERE email = $1 AND code = $2 \
         ORDER BY created_at DESC LIMIT 1"
    )
    .bind(&email)
    .bind(&code)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| ApiError::Internal(format!("DB error: {e}")))?;

    let (otp_id, used, expires_at) = row.ok_or_else(|| ApiError::Unauthorized("Invalid code".to_string()))?;
    if used {
        return Err(ApiError::Unauthorized("Code already used".to_string()));
    }
    if chrono::Utc::now() > expires_at {
        return Err(ApiError::Unauthorized("Code expired".to_string()));
    }

    // Mark as used
    sqlx::query("UPDATE otp_codes SET used = true WHERE id = $1")
        .bind(otp_id)
        .execute(&state.pool)
        .await
        .map_err(|e| ApiError::Internal(format!("DB error: {e}")))?;

    let user = db::find_user_by_email(&state.pool, &email)
        .await
        .ok_or_else(|| ApiError::Unauthorized("User not found".to_string()))?;

    let token = auth::issue_token(&user.id, &state.jwt_secret.0)
        .map_err(|e| ApiError::Internal(format!("Token error: {e}")))?;

    Ok(Json(AuthResponse {
        token,
        user_id: user.id.to_string(),
    }))
}

/// POST /api/v1/auth/signup
async fn signup(
    State(state): State<AppState>,
    Json(body): Json<AuthRequest>,
) -> Result<(StatusCode, Json<AuthResponse>), ApiError> {
    let email = body.email.trim().to_lowercase();
    if email.is_empty() || !email.contains('@') {
        return Err(ApiError::BadRequest("Invalid email".to_string()));
    }
    if body.password.len() < 8 {
        return Err(ApiError::BadRequest(
            "Password must be at least 8 characters".to_string(),
        ));
    }

    // Check if user already exists
    if db::find_user_by_email(&state.pool, &email).await.is_some() {
        return Err(ApiError::BadRequest("Email already registered".to_string()));
    }

    let password_hash = auth::hash_password(&body.password)
        .map_err(|e| ApiError::Internal(format!("Hash error: {e}")))?;

    let user_id = db::create_user(&state.pool, &email, &password_hash)
        .await
        .map_err(|e| ApiError::Internal(format!("DB error: {e}")))?;

    // Create a default store for the new user
    let store_name = body.store_name
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "")
        .to_string();
    let store_name = if store_name.is_empty() {
        format!("{}のお店", email.split('@').next().unwrap_or("user"))
    } else {
        store_name
    };
    if let Err(e) = db::create_default_store(&state.pool, &user_id, &store_name).await {
        warn!(error = %e, "Failed to create default store (non-fatal)");
    }

    // Send welcome email (fire-and-forget)
    {
        let email_clone = email.clone();
        let store_name_clone = store_name.clone();
        let resend_key = std::env::var("RESEND_API_KEY").ok();
        if let Some(key) = resend_key {
            let client = reqwest::Client::new();
            tokio::spawn(async move {
                let body = build_welcome_email(&store_name_clone);
                let _ = client
                    .post("https://api.resend.com/emails")
                    .header("Authorization", format!("Bearer {key}"))
                    .json(&serde_json::json!({
                        "from": "ミセバンAI <noreply@misebanai.com>",
                        "to": [&email_clone],
                        "subject": "【ミセバンAI】アカウント登録が完了しました",
                        "html": body,
                    }))
                    .send()
                    .await;
            });
        }
    }

    let token = auth::issue_token(&user_id, &state.jwt_secret.0)
        .map_err(|e| ApiError::Internal(format!("Token error: {e}")))?;

    Ok((
        StatusCode::CREATED,
        Json(AuthResponse {
            token,
            user_id: user_id.to_string(),
        }),
    ))
}

/// GET /api/v1/auth/me
///
/// Returns the authenticated user's profile info (id, email, store, plan).
async fn auth_me(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Look up the user record.
    let user_row: Option<(String,)> = sqlx::query_as("SELECT email FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| ApiError::Database(format!("DB error: {e}")))?;

    let email = user_row.map(|r| r.0).unwrap_or_default();

    // Fetch the user's store info if available.
    let store = db::get_store_by_owner(&state.pool, &user_id).await;

    let store_json = store.map(|s| {
        serde_json::json!({
            "id": s.id.to_string(),
            "name": s.name,
            "plan_tier": s.plan_tier,
        })
    });

    Ok(Json(serde_json::json!({
        "id": user_id.to_string(),
        "email": email,
        "store": store_json,
    })))
}

/// POST /api/v1/auth/login
async fn login(
    State(state): State<AppState>,
    Json(body): Json<AuthRequest>,
) -> Result<Json<AuthResponse>, ApiError> {
    let email = body.email.trim().to_lowercase();

    let user = db::find_user_by_email(&state.pool, &email)
        .await
        .ok_or_else(|| ApiError::Unauthorized("Invalid email or password".to_string()))?;

    if !auth::verify_password(&body.password, &user.password_hash) {
        return Err(ApiError::Unauthorized(
            "Invalid email or password".to_string(),
        ));
    }

    let token = auth::issue_token(&user.id, &state.jwt_secret.0)
        .map_err(|e| ApiError::Internal(format!("Token error: {e}")))?;

    Ok(Json(AuthResponse {
        token,
        user_id: user.id.to_string(),
    }))
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/// Server start time for uptime calculation.
static START_TIME: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Health check response.
#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    database: &'static str,
    uptime_seconds: u64,
}

/// GET /api/v1/health
///
/// Returns service health including database connectivity and uptime.
async fn health_check(State(state): State<AppState>) -> Json<HealthResponse> {
    let db_status = match sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.pool)
        .await
    {
        Ok(_) => "connected",
        Err(_) => "unavailable",
    };

    let uptime = START_TIME.get().map(|t| t.elapsed().as_secs()).unwrap_or(0);

    Json(HealthResponse {
        status: if db_status == "connected" {
            "ok"
        } else {
            "degraded"
        },
        version: env!("CARGO_PKG_VERSION"),
        database: db_status,
        uptime_seconds: uptime,
    })
}

// ---------------------------------------------------------------------------
// Contact form + Resend auto-reply
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ContactRequest {
    name: String,
    company: Option<String>,
    email: String,
    phone: Option<String>,
    #[serde(rename = "type")]
    contact_type: String,
    message: String,
}

/// POST /api/v1/contact
///
/// Receives a contact form submission, sends notification to the team and
/// an auto-reply to the sender via Resend.
async fn handle_contact(Json(req): Json<ContactRequest>) -> Result<impl IntoResponse, ApiError> {
    // Validate
    if req.name.trim().is_empty() || req.email.trim().is_empty() || req.message.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "name, email, message are required".into(),
        ));
    }
    if req.email.len() > 254 || !req.email.contains('@') {
        return Err(ApiError::BadRequest("Invalid email".into()));
    }

    let resend_key = std::env::var("RESEND_API_KEY").unwrap_or_default();
    if resend_key.is_empty() {
        warn!("RESEND_API_KEY not set — contact form will not send emails");
        return Ok((
            StatusCode::OK,
            Json(serde_json::json!({"ok": true, "note": "email delivery disabled"})),
        ));
    }

    let client = reqwest::Client::new();
    let contact_type = req.contact_type.as_str();

    // Determine notification recipient
    let notify_to = match contact_type {
        "partner" => "partners@miseban.ai",
        _ => "info@misebanai.com",
    };

    let type_label = match contact_type {
        "service" => "サービスに関するご質問",
        "trial" => "導入・トライアルのご相談",
        "estimate" => "お見積りのご依頼",
        "partner" => "パートナー提携のご相談",
        "press" => "取材・プレスのご依頼",
        _ => "その他",
    };

    // 1. Send notification to team
    let company_line = req.company.as_deref().unwrap_or("-");
    let phone_line = req.phone.as_deref().unwrap_or("-");
    let notify_body = format!(
        r#"<h2>📩 新しいお問い合わせ</h2>
<table style="border-collapse:collapse;width:100%">
<tr><td style="padding:8px;border:1px solid #ddd;font-weight:bold">種別</td><td style="padding:8px;border:1px solid #ddd">{type_label}</td></tr>
<tr><td style="padding:8px;border:1px solid #ddd;font-weight:bold">お名前</td><td style="padding:8px;border:1px solid #ddd">{}</td></tr>
<tr><td style="padding:8px;border:1px solid #ddd;font-weight:bold">会社名</td><td style="padding:8px;border:1px solid #ddd">{company_line}</td></tr>
<tr><td style="padding:8px;border:1px solid #ddd;font-weight:bold">メール</td><td style="padding:8px;border:1px solid #ddd">{}</td></tr>
<tr><td style="padding:8px;border:1px solid #ddd;font-weight:bold">電話</td><td style="padding:8px;border:1px solid #ddd">{phone_line}</td></tr>
</table>
<h3>メッセージ</h3>
<p style="white-space:pre-wrap;background:#f5f5f5;padding:16px;border-radius:8px">{}</p>"#,
        req.name, req.email, req.message
    );

    let notify_result = client
        .post("https://api.resend.com/emails")
        .header("Authorization", format!("Bearer {resend_key}"))
        .json(&serde_json::json!({
            "from": "ミセバンAI <noreply@misebanai.com>",
            "to": [notify_to],
            "subject": format!("[ミセバンAI] {} - {}", type_label, req.name),
            "html": notify_body,
            "reply_to": req.email,
        }))
        .send()
        .await;

    if let Err(e) = &notify_result {
        error!("Failed to send notification email: {e}");
    }

    // 2. Send auto-reply to sender
    let (reply_subject, reply_body) = build_auto_reply(contact_type, &req.name, company_line);

    let reply_result = client
        .post("https://api.resend.com/emails")
        .header("Authorization", format!("Bearer {resend_key}"))
        .json(&serde_json::json!({
            "from": "ミセバンAI <noreply@misebanai.com>",
            "to": [req.email],
            "subject": reply_subject,
            "html": reply_body,
            "reply_to": notify_to,
        }))
        .send()
        .await;

    if let Err(e) = &reply_result {
        error!("Failed to send auto-reply email: {e}");
    }

    info!(
        "Contact form: type={}, name={}, email={}, notify={}, reply={}",
        contact_type,
        req.name,
        req.email,
        notify_result.is_ok(),
        reply_result.is_ok()
    );

    Ok((StatusCode::OK, Json(serde_json::json!({"ok": true}))))
}

fn build_subscription_activated_email(store_name: &str, tier: &str) -> String {
    let tier_label = match tier {
        "pro" => "プロ",
        "enterprise" => "エンタープライズ",
        _ => "スターター",
    };
    format!(r#"<div style="font-family:sans-serif;max-width:600px;margin:0 auto;color:#1e293b">
<div style="background:linear-gradient(135deg,#4f46e5,#7c3aed);padding:32px;border-radius:12px 12px 0 0;text-align:center">
  <h1 style="color:white;margin:0;font-size:28px;font-weight:700">ミセバンAI</h1>
  <p style="color:rgba(255,255,255,0.85);margin:8px 0 0">AI店舗分析サービス</p>
</div>
<div style="padding:32px;background:white;border:1px solid #e2e8f0;border-top:none">
  <h2 style="color:#4f46e5;font-size:20px;margin-top:0">ご契約ありがとうございます 🎉</h2>
  <p>「{store_name}」が <strong>{tier_label}プラン</strong> に契約されました。</p>
  <div style="background:#f0fdf4;border:1px solid #bbf7d0;border-radius:8px;padding:16px;margin:20px 0">
    <p style="margin:0;color:#166534;font-size:14px">✅ プランが有効化されました。すべての機能をご利用いただけます。</p>
  </div>
  <div style="text-align:center;margin:28px 0">
    <a href="https://misebanai.com/dashboard/" style="background:#4f46e5;color:white;padding:14px 32px;border-radius:100px;text-decoration:none;font-weight:600;font-size:15px;display:inline-block">ダッシュボードを開く →</a>
  </div>
  <hr style="border:none;border-top:1px solid #e2e8f0;margin:24px 0">
  <p style="font-size:13px;color:#64748b">ご不明な点は <a href="mailto:info@misebanai.com" style="color:#4f46e5">info@misebanai.com</a> までご連絡ください。</p>
</div>
<div style="padding:16px;text-align:center;font-size:12px;color:#94a3b8">© 2026 ミセバンAI. <a href="https://misebanai.com/privacy.html" style="color:#94a3b8">プライバシーポリシー</a></div>
</div>"#, store_name = store_name, tier_label = tier_label)
}

fn build_subscription_cancelled_email(store_name: &str) -> String {
    format!(r#"<div style="font-family:sans-serif;max-width:600px;margin:0 auto;color:#1e293b">
<div style="background:linear-gradient(135deg,#4f46e5,#7c3aed);padding:32px;border-radius:12px 12px 0 0;text-align:center">
  <h1 style="color:white;margin:0;font-size:28px;font-weight:700">ミセバンAI</h1>
  <p style="color:rgba(255,255,255,0.85);margin:8px 0 0">AI店舗分析サービス</p>
</div>
<div style="padding:32px;background:white;border:1px solid #e2e8f0;border-top:none">
  <h2 style="color:#64748b;font-size:20px;margin-top:0">サブスクリプションの解約を承りました</h2>
  <p>「{store_name}」のサブスクリプションが解約されました。<br>現在の請求期間終了までは引き続きご利用いただけます。</p>
  <div style="background:#fef9c3;border:1px solid #fde047;border-radius:8px;padding:16px;margin:20px 0">
    <p style="margin:0;color:#854d0e;font-size:14px">ご利用いただきありがとうございました。またいつでもお待ちしております。</p>
  </div>
  <p style="font-size:14px;color:#475569">解約のお心当たりがない場合は <a href="mailto:info@misebanai.com" style="color:#4f46e5">info@misebanai.com</a> までご連絡ください。</p>
</div>
<div style="padding:16px;text-align:center;font-size:12px;color:#94a3b8">© 2026 ミセバンAI. <a href="https://misebanai.com/privacy.html" style="color:#94a3b8">プライバシーポリシー</a></div>
</div>"#, store_name = store_name)
}

fn build_payment_failed_email(store_name: &str) -> String {
    format!(r#"<div style="font-family:sans-serif;max-width:600px;margin:0 auto;color:#1e293b">
<div style="background:linear-gradient(135deg,#dc2626,#b91c1c);padding:32px;border-radius:12px 12px 0 0;text-align:center">
  <h1 style="color:white;margin:0;font-size:28px;font-weight:700">ミセバンAI</h1>
  <p style="color:rgba(255,255,255,0.85);margin:8px 0 0">AI店舗分析サービス</p>
</div>
<div style="padding:32px;background:white;border:1px solid #e2e8f0;border-top:none">
  <h2 style="color:#dc2626;font-size:20px;margin-top:0">⚠️ お支払いに問題が発生しました</h2>
  <p>「{store_name}」のサブスクリプション料金の引き落としに失敗しました。</p>
  <div style="background:#fef2f2;border:1px solid #fecaca;border-radius:8px;padding:16px;margin:20px 0">
    <p style="margin:0;color:#991b1b;font-size:14px">サービスを継続するには、お支払い情報を更新してください。</p>
  </div>
  <div style="text-align:center;margin:28px 0">
    <a href="https://misebanai.com/dashboard/" style="background:#dc2626;color:white;padding:14px 32px;border-radius:100px;text-decoration:none;font-weight:600;font-size:15px;display:inline-block">支払い情報を更新する →</a>
  </div>
  <p style="font-size:13px;color:#64748b">ご不明な点は <a href="mailto:info@misebanai.com" style="color:#4f46e5">info@misebanai.com</a> までご連絡ください。</p>
</div>
<div style="padding:16px;text-align:center;font-size:12px;color:#94a3b8">© 2026 ミセバンAI. <a href="https://misebanai.com/privacy.html" style="color:#94a3b8">プライバシーポリシー</a></div>
</div>"#, store_name = store_name)
}

fn build_welcome_email(store_name: &str) -> String {
    format!(r#"<div style="font-family:sans-serif;max-width:600px;margin:0 auto;color:#1e293b">
<div style="background:linear-gradient(135deg,#4f46e5,#7c3aed);padding:32px;border-radius:12px 12px 0 0;text-align:center">
  <h1 style="color:white;margin:0;font-size:28px;font-weight:700">ミセバンAI</h1>
  <p style="color:rgba(255,255,255,0.85);margin:8px 0 0;font-size:14px">AI店舗分析サービス</p>
</div>
<div style="padding:32px;background:white;border:1px solid #e2e8f0;border-top:none">
  <h2 style="color:#4f46e5;font-size:20px;margin-top:0">アカウント登録が完了しました 🎉</h2>
  <p>「{store_name}」のアカウントが作成されました。<br>まずはダッシュボードにアクセスして、カメラを接続してみましょう。</p>

  <div style="background:#f8fafc;border-radius:10px;padding:20px;margin:24px 0">
    <h3 style="margin-top:0;font-size:15px;color:#334155">🚀 はじめの3ステップ</h3>
    <ol style="margin:0;padding-left:20px;color:#475569;line-height:2">
      <li><strong>カメラを接続</strong> — 既存のIPカメラまたはRTSP対応カメラを登録</li>
      <li><strong>エージェントをインストール</strong> — Raspberry Pi / PC へワンコマンドで導入</li>
      <li><strong>データを確認</strong> — リアルタイムの来客数・属性分析を確認</li>
    </ol>
  </div>

  <div style="background:#eff6ff;border:1px solid #bfdbfe;border-radius:8px;padding:16px;margin:20px 0">
    <p style="margin:0;font-size:14px;color:#1e40af">
      🎁 <strong>Beta限定特典</strong>: 有料プランへのアップグレード時にコード <code style="background:#dbeafe;padding:2px 6px;border-radius:4px">MISEBAN30</code> を入力すると<strong>初月無料</strong>になります。
    </p>
  </div>

  <div style="text-align:center;margin:28px 0">
    <a href="https://misebanai.com/dashboard/" style="background:#4f46e5;color:white;padding:14px 32px;border-radius:100px;text-decoration:none;font-weight:600;font-size:15px;display:inline-block">ダッシュボードを開く →</a>
  </div>

  <hr style="border:none;border-top:1px solid #e2e8f0;margin:24px 0">
  <p style="font-size:13px;color:#64748b">
    ご不明な点は <a href="mailto:info@misebanai.com" style="color:#4f46e5">info@misebanai.com</a> までお気軽にご連絡ください。<br>
    <a href="https://misebanai.com/docs.html" style="color:#4f46e5">ドキュメント</a> | <a href="https://misebanai.com/download.html" style="color:#4f46e5">エージェントのダウンロード</a>
  </p>
</div>
<div style="padding:16px;text-align:center;font-size:12px;color:#94a3b8">
  © 2026 ミセバンAI. <a href="https://misebanai.com/privacy.html" style="color:#94a3b8">プライバシーポリシー</a>
</div>
</div>"#, store_name = store_name)
}

fn build_auto_reply(contact_type: &str, name: &str, company: &str) -> (String, String) {
    let header = format!(
        r#"<div style="font-family:sans-serif;max-width:600px;margin:0 auto;color:#1e293b">
<div style="background:linear-gradient(135deg,#4f46e5,#7c3aed);padding:32px;border-radius:12px 12px 0 0;text-align:center">
<h1 style="color:white;margin:0;font-size:24px">ミセバンAI</h1>
<p style="color:rgba(255,255,255,0.8);margin:8px 0 0">AI店舗分析サービス</p>
</div>
<div style="padding:32px;background:white;border:1px solid #e2e8f0;border-top:none">
<p>{name} 様</p>
<p>お問い合わせいただきありがとうございます。<br>以下の内容で承りました。担当者より{timeframe}にご連絡いたします。</p>"#,
        name = name,
        timeframe = match contact_type {
            "partner" | "press" => "2営業日以内",
            _ => "1営業日以内",
        }
    );

    let (subject, content) = match contact_type {
        "service" => (
            "【ミセバンAI】お問い合わせを受け付けました",
            r#"<h3 style="color:#4f46e5">🏪 ミセバンAIサービスのご紹介</h3>
<ul style="line-height:1.8">
<li><strong>AIカメラ分析</strong>: 来店者数、属性（年齢・性別）、滞留時間をリアルタイムで可視化</li>
<li><strong>ダッシュボード</strong>: 日次・週次・時間帯別レポートを自動生成</li>
<li><strong>アラート機能</strong>: 異常検知時にLINE/メールで即座に通知</li>
<li><strong>プライバシー</strong>: 映像はエッジ処理、個人情報は保存しません</li>
</ul>
<p>▶ <a href="https://misebanai.com/docs.html" style="color:#4f46e5">詳細ドキュメントはこちら</a></p>"#.to_string(),
        ),
        "trial" => (
            "【ミセバンAI】トライアルのご相談を受け付けました",
            r#"<h3 style="color:#4f46e5">🚀 導入までの流れ</h3>
<ol style="line-height:2">
<li><strong>ヒアリング</strong> — 店舗の課題と目標を伺います（30分程度）</li>
<li><strong>プラン提案</strong> — 最適なカメラ台数・設置場所をご提案</li>
<li><strong>設置・設定</strong> — 最短即日で稼働開始</li>
<li><strong>トライアル</strong> — 2週間の無料お試し期間</li>
</ol>
<p>▶ <a href="https://misebanai.com/cameras.html" style="color:#4f46e5">対応カメラ一覧</a><br>
▶ <a href="https://misebanai.com/" style="color:#4f46e5">料金プランを見る</a></p>"#.to_string(),
        ),
        "estimate" => (
            "【ミセバンAI】お見積りのご依頼を受け付けました",
            format!(
                r#"<h3 style="color:#4f46e5">💰 料金プラン</h3>
<table style="border-collapse:collapse;width:100%">
<tr style="background:#f8fafc"><th style="padding:12px;border:1px solid #e2e8f0;text-align:left">プラン</th><th style="padding:12px;border:1px solid #e2e8f0">月額</th><th style="padding:12px;border:1px solid #e2e8f0">カメラ数</th></tr>
<tr><td style="padding:12px;border:1px solid #e2e8f0">スターター</td><td style="padding:12px;border:1px solid #e2e8f0;text-align:center">¥4,980</td><td style="padding:12px;border:1px solid #e2e8f0;text-align:center">1台</td></tr>
<tr><td style="padding:12px;border:1px solid #e2e8f0">プロ</td><td style="padding:12px;border:1px solid #e2e8f0;text-align:center">¥14,800</td><td style="padding:12px;border:1px solid #e2e8f0;text-align:center">5台</td></tr>
<tr><td style="padding:12px;border:1px solid #e2e8f0">エンタープライズ</td><td style="padding:12px;border:1px solid #e2e8f0;text-align:center">お見積り</td><td style="padding:12px;border:1px solid #e2e8f0;text-align:center">無制限</td></tr>
</table>
<p>{company}様のご要件に合わせた詳細なお見積りを作成いたします。</p>"#, company = company),
        ),
        "partner" => (
            "【ミセバンAI】パートナーシップのお問い合わせを受け付けました",
            r#"<h3 style="color:#4f46e5">🤝 パートナープログラム</h3>
<p>ミセバンAIでは以下のパートナーシップを募集しています：</p>
<ul style="line-height:2">
<li><strong>販売代理店</strong> — 紹介手数料型のリセラープログラム</li>
<li><strong>技術提携</strong> — API連携によるOEM/ホワイトラベル提供</li>
<li><strong>カメラメーカー</strong> — 対応カメラの拡充</li>
<li><strong>不動産・商業施設</strong> — テナント向け一括導入</li>
</ul>
<p>パートナー担当より詳細資料をお送りいたします。</p>
<p>▶ パートナー専用窓口: <a href="mailto:partners@miseban.ai" style="color:#4f46e5">partners@miseban.ai</a></p>"#.to_string(),
        ),
        "press" => (
            "【ミセバンAI】プレスのお問い合わせを受け付けました",
            r#"<h3 style="color:#4f46e5">📰 プレス・取材について</h3>
<p>ミセバンAIへのご関心をいただきありがとうございます。</p>
<ul style="line-height:2">
<li>プレスキットは担当よりお送りいたします</li>
<li>代表インタビュー・デモのご要望も承ります</li>
<li>製品画像・ロゴデータもご提供可能です</li>
</ul>"#.to_string(),
        ),
        _ => (
            "【ミセバンAI】お問い合わせを受け付けました",
            r#"<p>お問い合わせ内容を確認の上、担当者よりご連絡いたします。</p>"#.to_string(),
        ),
    };

    let footer = r#"</div>
<div style="padding:24px;background:#f8fafc;border:1px solid #e2e8f0;border-top:none;border-radius:0 0 12px 12px;text-align:center;font-size:13px;color:#64748b">
<p>ミセバンAI — AI店舗分析サービス</p>
<p>
<a href="https://misebanai.com" style="color:#4f46e5">ウェブサイト</a> ・
<a href="mailto:info@misebanai.com" style="color:#4f46e5">info@misebanai.com</a> ・
<a href="mailto:partners@miseban.ai" style="color:#4f46e5">partners@miseban.ai</a>
</p>
</div></div>"#;

    let full_body = format!("{header}\n{content}\n{footer}");
    (subject.to_string(), full_body)
}

// ---------------------------------------------------------------------------
// Router builder (extracted for testability)
// ---------------------------------------------------------------------------

/// Public configuration for frontend clients.
#[derive(Serialize)]
struct PublicConfig {
    api_version: &'static str,
    supabase_url: String,
    features: Vec<&'static str>,
}

/// GET /api/v1/config
///
/// Returns public configuration for frontend clients (Supabase URL, API version, etc.).
/// Does NOT expose secret keys.
async fn get_public_config() -> Json<PublicConfig> {
    let supabase_url = std::env::var("SUPABASE_PUBLIC_URL")
        .or_else(|_| {
            std::env::var("DATABASE_URL").map(|db_url| {
                // Extract project ID from postgresql://...@db.XXXX.supabase.co:...
                if let Some(start) = db_url.find("db.") {
                    if let Some(end) = db_url[start..].find(".supabase.co") {
                        let project_id = &db_url[start + 3..start + end];
                        return format!("https://{}.supabase.co", project_id);
                    }
                }
                String::new()
            })
        })
        .unwrap_or_default();

    Json(PublicConfig {
        api_version: env!("CARGO_PKG_VERSION"),
        supabase_url,
        features: vec![
            "auth",
            "frames",
            "stats",
            "alerts",
            "billing",
            "csv_export",
            "line_notifications",
            "cameras",
            "weekly_stats",
            "hourly_stats",
            "pairing",
            "snapshot",
            "camera_tokens",
        ],
    })
}

// ---------------------------------------------------------------------------
// Camera snapshot upload (HTTP POST multipart — no edge device required)
// Cameras configure: POST https://api.misebanai.com/api/v1/camera/snapshot?token=<api_token>
// Form fields: `image` (JPEG file) + optional `camera_id` (text)
// Auth: ?token=<api_token>  OR  Authorization: Bearer <jwt>
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct SnapshotQuery {
    token: Option<String>,
    camera_id: Option<String>,
}

/// Resolve store_id from Bearer JWT or ?token= query param.
async fn resolve_store_from_auth(
    state: &AppState,
    auth_header: Option<&str>,
    token_param: Option<&str>,
) -> Result<Uuid, ApiError> {
    if let Some(bearer) = auth_header.and_then(|v| v.strip_prefix("Bearer ")) {
        use jsonwebtoken::{decode, DecodingKey, Validation};
        #[derive(serde::Deserialize)]
        struct Cl { sub: String }
        let mut val = Validation::new(jsonwebtoken::Algorithm::HS256);
        val.validate_aud = false;
        if let Ok(td) = decode::<Cl>(
            bearer,
            &DecodingKey::from_secret(state.jwt_secret.0.as_bytes()),
            &val,
        ) {
            let user_id = Uuid::parse_str(&td.claims.sub)
                .map_err(|_| ApiError::Unauthorized("Invalid JWT subject".to_string()))?;
            return db::get_store_id_by_owner(&state.pool, &user_id)
                .await
                .ok_or_else(|| ApiError::Unauthorized("No store for this user".to_string()));
        }
        // JWT decode failed — try as raw API token (Bearer <api_token> form)
        db::validate_api_token(&state.pool, bearer)
            .await
            .ok_or_else(|| ApiError::Unauthorized("Invalid token".to_string()))
    } else if let Some(raw) = token_param {
        db::validate_api_token(&state.pool, raw)
            .await
            .ok_or_else(|| ApiError::Unauthorized("Invalid API token".to_string()))
    } else {
        Err(ApiError::Unauthorized(
            "Provide Authorization: Bearer <jwt> or ?token=<api_token>".to_string(),
        ))
    }
}

/// POST /api/v1/camera/snapshot — multipart/form-data upload
/// Fields: `image` (JPEG), `camera_id` (text, optional)
async fn receive_snapshot(
    State(state): State<AppState>,
    Query(q): Query<SnapshotQuery>,
    headers: axum::http::HeaderMap,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    // Auth
    let auth_val = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let store_id =
        resolve_store_from_auth(&state, auth_val, q.token.as_deref()).await?;

    // Parse multipart fields
    let mut jpeg_bytes: Option<Vec<u8>> = None;
    let mut camera_id: Option<String> = None;

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        ApiError::BadRequest(format!("Multipart error: {e}"))
    })? {
        match field.name() {
            Some("image") | Some("snapshot") | Some("file") | Some("picture") => {
                let data = field.bytes().await.map_err(|e| {
                    ApiError::BadRequest(format!("Failed to read image: {e}"))
                })?;
                jpeg_bytes = Some(data.to_vec());
            }
            Some("camera_id") | Some("channel") | Some("deviceSerial") => {
                let text = field.text().await.map_err(|e| {
                    ApiError::BadRequest(format!("Failed to read camera_id: {e}"))
                })?;
                camera_id = Some(text);
            }
            _ => {}
        }
    }

    let jpeg = jpeg_bytes
        .ok_or_else(|| ApiError::BadRequest("Missing image field".to_string()))?;
    let cam_id = camera_id
        .or_else(|| q.camera_id.clone())
        .or_else(|| {
            headers.get("x-camera-id")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "camera-1".to_string());

    snapshot_process(&state, store_id, cam_id, jpeg).await
}

/// Shared logic: AI analysis + DB persist for snapshot uploads.
async fn snapshot_process(
    state: &AppState,
    store_id: Uuid,
    camera_id: String,
    jpeg_bytes: Vec<u8>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    const MAX_JPEG: usize = 10 * 1024 * 1024;
    if jpeg_bytes.is_empty() {
        return Err(ApiError::BadRequest("Empty image data".to_string()));
    }
    if jpeg_bytes.len() > MAX_JPEG {
        return Err(ApiError::BadRequest("Image too large (max 10MB)".to_string()));
    }
    if camera_id.is_empty() || camera_id.len() > 128 {
        return Err(ApiError::BadRequest("Invalid camera_id".to_string()));
    }

    info!(store_id = %store_id, camera_id = %camera_id, bytes = jpeg_bytes.len(), "Snapshot received");

    let frame = shared::FrameData {
        camera_id: camera_id.clone(),
        timestamp: chrono::Utc::now(),
        jpeg_bytes,
        resolution: shared::Resolution { width: 0, height: 0 },
    };
    let result = ai::analyze_frame(&frame).await;

    let camera_uuid = match Uuid::parse_str(&camera_id) {
        Ok(uuid) => Some(uuid),
        Err(_) => match db::find_camera_by_name(&state.pool, &store_id, &camera_id).await {
            Some(uuid) => Some(uuid),
            None => match db::register_camera(&state.pool, &store_id, &camera_id).await {
                Ok(uuid) => { info!(camera_id = %camera_id, "Auto-registered camera"); Some(uuid) }
                Err(e) => { warn!(error = %e, "Camera auto-register failed (non-fatal)"); None }
            },
        },
    };

    if let Some(ref cam_uuid) = camera_uuid {
        let demo_json = serde_json::to_value(&result.demographics).unwrap_or_default();
        let zones_json = serde_json::to_value(&result.zones).unwrap_or_default();
        if let Err(e) = db::insert_visitor_count(&state.pool, cam_uuid, result.people_count as i32, demo_json, zones_json).await {
            warn!(error = %e, "Failed to persist snapshot count (non-fatal)");
        }
        for pa in &alerts::evaluate_alerts(&result) {
            let _ = alerts::insert_alert(&state.pool, &store_id, cam_uuid, &pa.alert_type, pa.confidence, &pa.message).await;
        }
    }

    Ok((StatusCode::OK, Json(serde_json::json!({
        "camera_id": camera_id,
        "people_count": result.people_count,
        "timestamp": result.timestamp,
    }))))
}

// ---------------------------------------------------------------------------
// API token management
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct ApiTokenResponse {
    id: uuid::Uuid,
    name: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    last_used_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl From<db::ApiTokenRow> for ApiTokenResponse {
    fn from(r: db::ApiTokenRow) -> Self {
        Self {
            id: r.id,
            name: r.name,
            created_at: r.created_at,
            expires_at: r.expires_at,
            last_used_at: r.last_used_at,
        }
    }
}

#[derive(serde::Deserialize)]
struct CreateTokenRequest {
    name: Option<String>,
}

/// POST /api/v1/stores/me/tokens — create a new API token
async fn create_token(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Json(body): Json<CreateTokenRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("Store not found".to_string()))?;

    let (raw_token, row) =
        db::create_api_token(&state.pool, &store.id, body.name.as_deref())
            .await
            .map_err(|e| ApiError::Database(e.to_string()))?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "token": raw_token,  // returned only once
            "id": row.id,
            "name": row.name,
            "created_at": row.created_at,
            "note": "Store this token securely — it will not be shown again."
        })),
    ))
}

/// GET /api/v1/stores/me/tokens — list API tokens (without raw values)
async fn list_tokens(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("Store not found".to_string()))?;

    let tokens: Vec<ApiTokenResponse> = db::list_api_tokens(&state.pool, &store.id)
        .await
        .into_iter()
        .map(ApiTokenResponse::from)
        .collect();

    Ok(Json(serde_json::json!({ "tokens": tokens })))
}

/// DELETE /api/v1/stores/me/tokens/:token_id
async fn delete_token(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Path(token_id): Path<uuid::Uuid>,
) -> Result<StatusCode, ApiError> {
    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("Store not found".to_string()))?;

    if db::delete_api_token(&state.pool, &store.id, &token_id).await {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound("Token not found".to_string()))
    }
}

/// Build the application router with all routes and middleware.
///
/// Separated from `main` so integration tests can construct the same router
/// without starting a TCP listener or reading environment variables.
fn build_router(state: AppState) -> Router {
    // CORS: allow the deployed frontend and localhost for development.
    let cors = CorsLayer::new()
        .allow_origin([
            "https://misebanai.com"
                .parse::<HeaderValue>()
                .expect("valid origin"),
            "https://www.misebanai.com"
                .parse::<HeaderValue>()
                .expect("valid origin"),
            "https://miseban-ai.fly.dev"
                .parse::<HeaderValue>()
                .expect("valid origin"),
            "http://localhost:3001"
                .parse::<HeaderValue>()
                .expect("valid origin"),
        ])
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]);

    Router::new()
        // Authenticated routes
        .route("/api/v1/frames", post(receive_frame))
        .route("/api/v1/stores/me/stats", get(get_my_store_stats))
        .route("/api/v1/stores/me/stats/weekly", get(get_weekly_stats))
        .route("/api/v1/stores/me/stats/hourly", get(get_hourly_stats))
        .route("/api/v1/stores/me/daily", get(get_my_daily_report))
        .route("/api/v1/stores/me/cameras", get(get_my_cameras).post(create_my_camera))
        .route("/api/v1/stores/me/cameras/:camera_id", delete(delete_my_camera))
        .route("/api/v1/agent/config", get(agent_config))
        .route("/api/v1/agent/heartbeat", post(agent_heartbeat))
        .route("/api/v1/stores/me/usage", get(get_my_usage))
        .route("/api/v1/stores/me/export/csv", get(export_csv))
        .route("/api/v1/stores/me/alerts", get(get_my_alerts))
        .route("/api/v1/stores/me/alerts/count", get(get_my_alert_count))
        .route("/api/v1/alerts/:alert_id/read", patch(mark_alert_read))
        .route("/api/v1/alerts/read-all", post(mark_all_alerts_read))
        .route("/api/v1/stores/:store_id/stats", get(get_store_stats))
        .route("/api/v1/stores/:store_id/daily", get(get_daily_report))
        // Billing routes (auth required)
        .route("/api/v1/billing/checkout", post(create_checkout))
        .route("/api/v1/billing/portal", post(create_portal))
        .route("/api/v1/billing/subscription", get(get_subscription))
        // Camera snapshot upload (HTTP POST — no edge device required)
        .route("/api/v1/camera/snapshot", post(receive_snapshot))
        // API token management (for cameras that can't use JWT)
        .route("/api/v1/stores/me/tokens", post(create_token).get(list_tokens))
        .route("/api/v1/stores/me/tokens/:token_id", delete(delete_token))
        // Pairing routes
        .route("/api/v1/pair", post(handle_pair)) // public (agent setup)
        .route("/api/v1/pair/generate", post(generate_pairing_code)) // auth required
        // Auth routes (public)
        .route("/api/v1/auth/send-otp", post(send_otp))
        .route("/api/v1/auth/magic", get(magic_login))
        .route("/api/v1/auth/verify-otp", post(verify_otp))
        .route("/api/v1/auth/signup", post(signup))
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/auth/me", get(auth_me))
        // Public routes
        .route("/api/v1/health", get(health_check))
        .route("/api/v1/config", get(get_public_config))
        .route("/api/v1/pricing", get(get_pricing))
        .route("/api/v1/contact", post(handle_contact))
        .route("/api/v1/webhooks/line", post(line_webhook))
        .route("/api/v1/webhooks/stripe", post(stripe_webhook))
        .layer(axum::extract::DefaultBodyLimit::max(12 * 1024 * 1024)) // 12MB max
        .layer(cors)
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    // Record server start time for uptime tracking.
    START_TIME.get_or_init(std::time::Instant::now);

    // Load .env file if present (non-fatal if missing).
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Read configuration from environment.
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let jwt_secret = std::env::var("SUPABASE_JWT_SECRET").expect("SUPABASE_JWT_SECRET must be set");
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    // Optional: LINE Messaging API token.
    let line_client = match std::env::var("LINE_CHANNEL_TOKEN") {
        Ok(token) if !token.is_empty() => {
            info!("LINE_CHANNEL_TOKEN found — LINE notifications enabled");
            OptionalLineClient(Some(line::LineClient::new(&token)))
        }
        _ => {
            info!("LINE_CHANNEL_TOKEN not set — LINE notifications disabled");
            OptionalLineClient(None)
        }
    };

    // Optional: Stripe integration.
    let stripe_client = match std::env::var("STRIPE_SECRET_KEY") {
        Ok(key) if !key.is_empty() => {
            info!("STRIPE_SECRET_KEY found — Stripe billing enabled");
            OptionalStripeClient(Some(billing::StripeClient::new(&key)))
        }
        _ => {
            info!("STRIPE_SECRET_KEY not set — Stripe billing disabled");
            OptionalStripeClient(None)
        }
    };

    // Read optional Stripe webhook secret (for signature verification in production).
    if std::env::var("STRIPE_WEBHOOK_SECRET").is_ok() {
        info!("STRIPE_WEBHOOK_SECRET found — webhook signature verification available");
    }

    // Create database connection pool.
    info!("Connecting to database...");
    let pool = db::create_pool(&database_url)
        .await
        .expect("Failed to create database pool");
    info!("Database connection pool created (max_connections=5)");

    // Initialize AI model (no-op if model file not found; falls back to 0-count).
    if let Err(e) = ai::init_model() {
        warn!(error = %e, "AI model init failed (inference will return 0 people)");
    }

    let gemini_key = std::env::var("GEMINI_API_KEY")
        .or_else(|_| std::env::var("GOOGLE_API_KEY"))
        .ok();
    if gemini_key.is_some() {
        info!("GEMINI_API_KEY configured — age/gender demographics enabled");
    } else {
        info!("No GEMINI_API_KEY — demographics will show Unknown");
    }

    let state = AppState {
        pool,
        jwt_secret: JwtSecret(jwt_secret),
        line_client,
        stripe_client,
        rate_limiter: plan_guard::RateLimiter::new(),
        trackers: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        gemini_key,
    };

    let app = build_router(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!("MisebanAI API server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("Failed to bind to address");

    axum::serve(listener, app).await.expect("Server error");
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt; // for oneshot

    /// Build a test router backed by a lazy (non-connecting) PgPool.
    ///
    /// Public endpoints (health, pricing) never touch the database, so a lazy
    /// pool is sufficient.  Authenticated endpoints that require a DB will
    /// fail at the auth layer before any query is executed.
    fn test_app() -> Router {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/miseban_test_fake")
            .expect("Failed to create lazy pool");

        let state = AppState {
            pool,
            jwt_secret: JwtSecret("test-secret-for-integration-tests".to_string()),
            line_client: OptionalLineClient(None),
            stripe_client: OptionalStripeClient(None),
            rate_limiter: plan_guard::RateLimiter::new(),
            trackers: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            gemini_key: None,
        };

        build_router(state)
    }

    #[tokio::test]
    async fn health_check_returns_200() {
        let app = test_app();

        let request = Request::builder()
            .uri("/api/v1/health")
            .method("GET")
            .body(axum::body::Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

        // Status will be "degraded" in test (no real DB), but endpoint works.
        assert!(
            body["status"].is_string(),
            "status field should be a string"
        );
        assert!(
            body["version"].is_string(),
            "version field should be a string"
        );
        assert!(
            body["database"].is_string(),
            "database field should be a string"
        );
    }

    #[tokio::test]
    async fn pricing_returns_valid_json_with_tiers() {
        let app = test_app();

        let request = Request::builder()
            .uri("/api/v1/pricing")
            .method("GET")
            .body(axum::body::Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let plans: Vec<serde_json::Value> = serde_json::from_slice(&body_bytes).unwrap();

        // The pricing endpoint should return at least 3 pricing tiers.
        assert!(
            plans.len() >= 3,
            "Expected at least 3 pricing tiers, got {}",
            plans.len()
        );

        // Verify each plan has the expected fields.
        for plan in &plans {
            assert!(plan["tier"].is_string(), "plan should have a tier field");
            assert!(plan["name"].is_string(), "plan should have a name field");
            assert!(
                plan["price_monthly"].is_number(),
                "plan should have a price_monthly field"
            );
            assert!(
                plan["features"].is_array(),
                "plan should have a features array"
            );
        }

        // Verify known tier names are present.
        let tier_names: Vec<&str> = plans.iter().filter_map(|p| p["tier"].as_str()).collect();
        assert!(tier_names.contains(&"free"), "should contain free tier");
        assert!(
            tier_names.contains(&"starter"),
            "should contain starter tier"
        );
        assert!(tier_names.contains(&"pro"), "should contain pro tier");
    }

    #[tokio::test]
    async fn frames_without_auth_returns_401() {
        let app = test_app();

        // POST to /api/v1/frames with a valid JSON body but no Authorization header.
        let request = Request::builder()
            .uri("/api/v1/frames")
            .method("POST")
            .header("content-type", "application/json")
            .body(axum::body::Body::from("{}"))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

        assert!(
            body["error"].is_string(),
            "Unauthorized response should contain an error message"
        );
    }

    #[test]
    fn stripe_signature_verification_works() {
        let secret = "whsec_test_secret_key";
        let payload = r#"{"type":"checkout.session.completed","data":{}}"#;
        let timestamp = chrono::Utc::now().timestamp().to_string();

        // Compute valid signature
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;
        let signed = format!("{}.{}", timestamp, payload);
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(signed.as_bytes());
        let sig = hex::encode(mac.finalize().into_bytes());
        let header = format!("t={},v1={}", timestamp, sig);

        // Valid signature should pass
        assert!(billing::verify_stripe_signature(payload, &header, secret).is_ok());

        // Wrong signature should fail
        let bad_header = format!("t={},v1=deadbeef", timestamp);
        assert!(billing::verify_stripe_signature(payload, &bad_header, secret).is_err());

        // Missing timestamp should fail
        assert!(billing::verify_stripe_signature(payload, "v1=abc", secret).is_err());
    }

    #[test]
    fn determine_tier_from_metadata() {
        let session = serde_json::json!({
            "metadata": { "store_id": "abc", "tier": "pro" },
            "customer": "cus_123"
        });
        assert_eq!(billing::determine_tier(&session), "pro");
    }

    #[test]
    fn determine_tier_from_amount() {
        let session = serde_json::json!({
            "metadata": { "store_id": "abc" },
            "customer": "cus_123",
            "amount_total": 29800
        });
        assert_eq!(billing::determine_tier(&session), "pro");
    }

    #[test]
    fn line_signature_verification_works() {
        let secret = "test_channel_secret";
        let body = b"{\"events\":[]}";

        // Compute valid signature
        use base64::Engine;
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

        assert!(line::verify_line_signature(body, &sig, secret));
        assert!(!line::verify_line_signature(body, "invalid_sig", secret));
    }

    #[tokio::test]
    async fn config_returns_api_version_and_features() {
        let app = test_app();

        let request = Request::builder()
            .uri("/api/v1/config")
            .method("GET")
            .body(axum::body::Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let config: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

        assert!(config["api_version"].is_string(), "should have api_version");
        assert!(config["features"].is_array(), "should have features array");

        let features: Vec<&str> = config["features"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(features.contains(&"frames"), "should list frames feature");
        assert!(
            features.contains(&"weekly_stats"),
            "should list weekly_stats"
        );
        assert!(
            features.contains(&"hourly_stats"),
            "should list hourly_stats"
        );
    }

    #[tokio::test]
    async fn signup_rejects_invalid_email() {
        let app = test_app();

        let request = Request::builder()
            .uri("/api/v1/auth/signup")
            .method("POST")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"email":"bademail","password":"12345678"}"#,
            ))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn signup_rejects_short_password() {
        let app = test_app();

        let request = Request::builder()
            .uri("/api/v1/auth/signup")
            .method("POST")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"email":"test@example.com","password":"short"}"#,
            ))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn login_returns_401_for_nonexistent_user() {
        let app = test_app();

        let request = Request::builder()
            .uri("/api/v1/auth/login")
            .method("POST")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"email":"nobody@example.com","password":"password123"}"#,
            ))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        // Will be 401 (user not found) or 500 (DB unavailable) — both are acceptable
        assert!(
            response.status() == StatusCode::UNAUTHORIZED
                || response.status() == StatusCode::INTERNAL_SERVER_ERROR,
            "Expected 401 or 500, got {}",
            response.status()
        );
    }

    #[tokio::test]
    async fn authenticated_endpoints_require_auth() {
        // Test multiple authenticated endpoints return 401 without token
        let endpoints = vec![
            ("/api/v1/stores/me/stats", "GET"),
            ("/api/v1/stores/me/stats/weekly", "GET"),
            ("/api/v1/stores/me/stats/hourly", "GET"),
            ("/api/v1/stores/me/daily", "GET"),
            ("/api/v1/stores/me/cameras", "GET"),
            ("/api/v1/stores/me/usage", "GET"),
            ("/api/v1/stores/me/alerts", "GET"),
            ("/api/v1/stores/me/alerts/count", "GET"),
            ("/api/v1/stores/me/export/csv", "GET"),
            ("/api/v1/auth/me", "GET"),
        ];

        for (uri, method) in endpoints {
            let app = test_app();
            let request = Request::builder()
                .uri(uri)
                .method(method)
                .body(axum::body::Body::empty())
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{} {} should return 401 without auth",
                method,
                uri
            );
        }
    }

    #[tokio::test]
    async fn pair_rejects_invalid_code_format() {
        let app = test_app();

        // Too short
        let request = Request::builder()
            .uri("/api/v1/pair")
            .method("POST")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(r#"{"code":"123"}"#))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn pair_rejects_non_numeric_code() {
        let app = test_app();

        let request = Request::builder()
            .uri("/api/v1/pair")
            .method("POST")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(r#"{"code":"abcdef"}"#))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn pair_generate_requires_auth() {
        let app = test_app();

        let request = Request::builder()
            .uri("/api/v1/pair/generate")
            .method("POST")
            .body(axum::body::Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
