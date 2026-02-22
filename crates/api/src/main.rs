use std::net::SocketAddr;

use axum::{
    extract::Path,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use serde::Serialize;
use shared::{
    AgeDistribution, AgeGroup, AnalysisResult, DailyReport, DemographicsSummary, FrameData,
    GenderDistribution, GenderEstimate,
};
use tower_http::cors::{Any, CorsLayer};
use tracing::info;


// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// POST /api/v1/frames
///
/// Receives a frame from an agent, runs AI analysis, returns the result.
async fn receive_frame(Json(frame): Json<FrameData>) -> (StatusCode, Json<AnalysisResult>) {
    info!(
        camera_id = %frame.camera_id,
        timestamp = %frame.timestamp,
        "Frame received"
    );

    // Run AI inference (placeholder).
    let result = ai::analyze_frame(&frame).await;

    (StatusCode::OK, Json(result))
}

/// Response wrapper for store stats.
#[derive(Serialize)]
struct StoreStats {
    store_id: String,
    current_visitors: u32,
    today_total: u64,
    cameras_online: u32,
}

/// GET /api/v1/stores/:store_id/stats
async fn get_store_stats(Path(store_id): Path<String>) -> Json<StoreStats> {
    info!(store_id = %store_id, "Stats requested");

    // Placeholder mock data.
    Json(StoreStats {
        store_id,
        current_visitors: 12,
        today_total: 347,
        cameras_online: 2,
    })
}

/// GET /api/v1/stores/:store_id/daily
async fn get_daily_report(Path(store_id): Path<String>) -> Json<DailyReport> {
    info!(store_id = %store_id, "Daily report requested");

    // Placeholder mock data.
    let today = Utc::now().date_naive();
    Json(DailyReport {
        store_id,
        date: today,
        total_visitors: 347,
        peak_hour: 14,
        demographics_summary: DemographicsSummary {
            age_distribution: vec![
                AgeDistribution {
                    age_group: AgeGroup::Child,
                    percentage: 5.0,
                },
                AgeDistribution {
                    age_group: AgeGroup::Teen,
                    percentage: 10.0,
                },
                AgeDistribution {
                    age_group: AgeGroup::YoungAdult,
                    percentage: 30.0,
                },
                AgeDistribution {
                    age_group: AgeGroup::Adult,
                    percentage: 40.0,
                },
                AgeDistribution {
                    age_group: AgeGroup::Senior,
                    percentage: 15.0,
                },
            ],
            gender_distribution: vec![
                GenderDistribution {
                    gender: GenderEstimate::Male,
                    percentage: 48.0,
                },
                GenderDistribution {
                    gender: GenderEstimate::Female,
                    percentage: 47.0,
                },
                GenderDistribution {
                    gender: GenderEstimate::Unknown,
                    percentage: 5.0,
                },
            ],
        },
    })
}

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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/api/v1/frames", post(receive_frame))
        .route("/api/v1/stores/:store_id/stats", get(get_store_stats))
        .route("/api/v1/stores/:store_id/daily", get(get_daily_report))
        .route("/api/v1/health", get(health_check))
        .layer(cors);

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    info!("MisebanAI API server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("Failed to bind to address");

    axum::serve(listener, app)
        .await
        .expect("Server error");
}
