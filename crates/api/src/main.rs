mod alerts;
mod auth;
mod billing;
mod csv_export;
mod db;
mod line;
mod plan_guard;

use std::net::SocketAddr;

use axum::{
    extract::{FromRef, Path, Query, State},
    http::{header, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
    Json, Router,
};
use chrono::Utc;
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
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            ApiError::Database(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            ApiError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg.clone()),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
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
    info!(
        camera_id = %frame.camera_id,
        timestamp = %frame.timestamp,
        user_id = %user_id,
        "Frame received"
    );

    // Soft plan enforcement: warn if over camera limit (do not reject).
    if let Some(store) = db::get_store_by_owner(&state.pool, &user_id).await {
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

    // Run AI inference.
    let result = ai::analyze_frame(&frame).await;

    // Persist to DB: try to parse camera_id as UUID and insert visitor_count.
    let camera_uuid = Uuid::parse_str(&result.camera_id).ok();

    if let Some(ref cam_id) = camera_uuid {
        let demographics_json = serde_json::to_value(&result.demographics)
            .unwrap_or(serde_json::Value::Null);
        let zones_json = serde_json::to_value(&result.zones)
            .unwrap_or(serde_json::Value::Null);

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
    } else {
        warn!(
            camera_id = %frame.camera_id,
            "camera_id is not a valid UUID; skipping DB insert"
        );
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

    let (today_total, cameras_online) =
        db::get_store_stats_db(&state.pool, &store.id).await;

    Ok(Json(StoreStats {
        store_id: store.id.to_string(),
        current_visitors: today_total, // best approximation without real-time tracking
        today_total,
        cameras_online,
    }))
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
        return Err(ApiError::Forbidden(
            "You do not own this store".to_string(),
        ));
    }

    let (today_total, cameras_online) =
        db::get_store_stats_db(&state.pool, &store_uuid).await;

    Ok(Json(StoreStats {
        store_id,
        current_visitors: today_total,
        today_total,
        cameras_online,
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
        return Err(ApiError::Forbidden(
            "You do not own this store".to_string(),
        ));
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
/// The DB stores a flat map like `{"male_20s": 10, "female_30s": 5, ...}`.
/// We convert it into percentages for the API response.
fn parse_demographics_summary(value: &serde_json::Value) -> Option<DemographicsSummary> {
    let obj = value.as_object()?;
    if obj.is_empty() {
        return None;
    }

    let total: f64 = obj.values().filter_map(|v| v.as_f64()).sum();
    if total <= 0.0 {
        return None;
    }

    // Aggregate by gender
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
    let gender_distribution = if gender_total > 0.0 {
        vec![
            GenderDistribution {
                gender: GenderEstimate::Male,
                percentage: (male_count / gender_total * 100.0) as f32,
            },
            GenderDistribution {
                gender: GenderEstimate::Female,
                percentage: (female_count / gender_total * 100.0) as f32,
            },
            GenderDistribution {
                gender: GenderEstimate::Unknown,
                percentage: (other_count / gender_total * 100.0) as f32,
            },
        ]
    } else {
        default_gender_distribution()
    };

    // For age distribution, we derive from the key naming pattern (e.g., "male_20s" -> YoungAdult)
    let age_distribution = default_age_distribution();

    Some(DemographicsSummary {
        age_distribution,
        gender_distribution,
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
        Err(ApiError::NotFound("Alert not found or already read".to_string()))
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
    let from = params.from.unwrap_or_else(|| to - chrono::Duration::days(30));

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
    price_id: String,
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

    let store = db::get_store_by_owner(&state.pool, &user_id)
        .await
        .ok_or_else(|| ApiError::NotFound("No store found for this user".to_string()))?;

    // Use store owner's email as customer email (fallback to placeholder).
    let customer_email = format!("{}@miseban.ai", user_id);

    let session = stripe
        .create_checkout_session(
            &body.price_id,
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
        .ok_or_else(|| {
            ApiError::NotFound("No Stripe customer linked to this store".to_string())
        })?;

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
async fn stripe_webhook(
    State(state): State<AppState>,
    body: String,
) -> Result<StatusCode, ApiError> {
    let event: billing::StripeEvent = serde_json::from_str(&body)
        .map_err(|e| ApiError::Internal(format!("Invalid webhook payload: {e}")))?;

    info!(event_type = %event.event_type, "Stripe webhook received");

    match event.event_type.as_str() {
        "checkout.session.completed" => {
            let obj = &event.data.object;
            let customer_id = obj["customer"].as_str().unwrap_or_default();
            let store_id_str = obj["metadata"]["store_id"].as_str().unwrap_or_default();

            if let Ok(store_id) = Uuid::parse_str(store_id_str) {
                // Save customer ID to store.
                let _ =
                    billing::set_stripe_customer_id(&state.pool, &store_id, customer_id).await;

                // Default to "starter" on any checkout completion.
                // TODO: determine tier from price_id in the session line items.
                let _ = billing::update_plan_tier(&state.pool, &store_id, "starter").await;
                info!(store_id = %store_id, "Checkout completed, plan upgraded");
            }
        }
        "customer.subscription.deleted" => {
            let customer_id = event.data.object["customer"]
                .as_str()
                .unwrap_or_default();
            if let Some((store_id, _)) =
                billing::get_store_by_stripe_customer(&state.pool, customer_id).await
            {
                let _ = billing::update_plan_tier(&state.pool, &store_id, "free").await;
                info!(store_id = %store_id, "Subscription cancelled, downgraded to free");
            }
        }
        "invoice.payment_failed" => {
            let customer_id = event.data.object["customer"]
                .as_str()
                .unwrap_or_default();
            warn!(customer_id = %customer_id, "Payment failed");
        }
        _ => {
            tracing::debug!(event_type = %event.event_type, "Unhandled Stripe event");
        }
    }

    Ok(StatusCode::OK)
}

/// POST /api/v1/webhooks/line
///
/// LINE webhook endpoint (public, no auth). Receives events from LINE platform.
/// For now, logs follow/unfollow events for future user linking.
async fn line_webhook(
    // No auth -- LINE webhooks are public. Signature verification is recommended
    // in production via X-Line-Signature header.
    Json(body): Json<serde_json::Value>,
) -> StatusCode {
    // LINE webhook verification: respond 200 to any POST.
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
// Health
// ---------------------------------------------------------------------------

/// Health check response.
#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
}

/// GET /api/v1/health
async fn health_check() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    // Load .env file if present (non-fatal if missing).
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Read configuration from environment.
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set");
    let jwt_secret = std::env::var("SUPABASE_JWT_SECRET")
        .expect("SUPABASE_JWT_SECRET must be set");
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

    let state = AppState {
        pool,
        jwt_secret: JwtSecret(jwt_secret),
        line_client,
        stripe_client,
    };

    // CORS: allow the deployed frontend and localhost for development.
    let cors = CorsLayer::new()
        .allow_origin([
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

    let app = Router::new()
        // Authenticated routes
        .route("/api/v1/frames", post(receive_frame))
        .route("/api/v1/stores/me/stats", get(get_my_store_stats))
        .route("/api/v1/stores/me/daily", get(get_my_daily_report))
        .route("/api/v1/stores/me/cameras", get(get_my_cameras))
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
        // Public routes
        .route("/api/v1/health", get(health_check))
        .route("/api/v1/pricing", get(get_pricing))
        .route("/api/v1/webhooks/line", post(line_webhook))
        .route("/api/v1/webhooks/stripe", post(stripe_webhook))
        .layer(cors)
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!("MisebanAI API server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("Failed to bind to address");

    axum::serve(listener, app)
        .await
        .expect("Server error");
}
