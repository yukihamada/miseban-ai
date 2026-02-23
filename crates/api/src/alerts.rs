use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shared::{AlertType, AnalysisResult};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Row struct (mapped from alerts table)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
pub struct AlertRow {
    pub id: Uuid,
    pub store_id: Uuid,
    pub camera_id: Option<Uuid>,
    pub alert_type: String,
    pub confidence: Option<f32>,
    pub message: Option<String>,
    pub is_read: bool,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// API payload (serialized for JSON responses)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertPayload {
    pub id: String,
    pub store_id: String,
    pub camera_id: String,
    pub alert_type: String,
    pub confidence: f32,
    pub message: String,
    pub is_read: bool,
    pub created_at: String,
}

impl From<AlertRow> for AlertPayload {
    fn from(row: AlertRow) -> Self {
        Self {
            id: row.id.to_string(),
            store_id: row.store_id.to_string(),
            camera_id: row.camera_id.map(|id| id.to_string()).unwrap_or_default(),
            alert_type: row.alert_type,
            confidence: row.confidence.unwrap_or(0.0),
            message: row.message.unwrap_or_default(),
            is_read: row.is_read,
            created_at: row.created_at.to_rfc3339(),
        }
    }
}

// ---------------------------------------------------------------------------
// Pending alert (pre-insert, from AI evaluation)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PendingAlert {
    pub alert_type: String,
    pub confidence: f32,
    pub message: String,
}

// ---------------------------------------------------------------------------
// Alert evaluation (AI result -> pending alerts)
// ---------------------------------------------------------------------------

/// Examine an AnalysisResult and convert any fired alerts into DB-insertable format.
pub fn evaluate_alerts(analysis: &AnalysisResult) -> Vec<PendingAlert> {
    analysis
        .alerts
        .iter()
        .map(|a| {
            let type_str = match a.alert_type {
                AlertType::Intrusion => "intrusion",
                AlertType::Unusual => "unusual",
                AlertType::Crowding => "crowding",
            };
            PendingAlert {
                alert_type: type_str.to_string(),
                confidence: a.confidence,
                message: a
                    .message
                    .clone()
                    .unwrap_or_else(|| format!("{} alert detected", type_str)),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// DB query functions
// ---------------------------------------------------------------------------

/// Insert a new alert record and return the created row.
pub async fn insert_alert(
    pool: &PgPool,
    store_id: &Uuid,
    camera_id: &Uuid,
    alert_type: &str,
    confidence: f32,
    message: &str,
) -> Result<AlertRow, sqlx::Error> {
    sqlx::query_as::<_, AlertRow>(
        "INSERT INTO alerts (store_id, camera_id, alert_type, confidence, message) \
         VALUES ($1, $2, $3, $4, $5) \
         RETURNING id, store_id, camera_id, alert_type, confidence, message, is_read, created_at",
    )
    .bind(store_id)
    .bind(camera_id)
    .bind(alert_type)
    .bind(confidence)
    .bind(message)
    .fetch_one(pool)
    .await
}

/// Fetch recent alerts for a store, optionally filtered to unread only.
pub async fn get_recent_alerts(
    pool: &PgPool,
    store_id: &Uuid,
    limit: i64,
    unread_only: bool,
) -> Vec<AlertRow> {
    if unread_only {
        sqlx::query_as::<_, AlertRow>(
            "SELECT id, store_id, camera_id, alert_type, confidence, message, is_read, created_at \
             FROM alerts \
             WHERE store_id = $1 AND is_read = false \
             ORDER BY created_at DESC \
             LIMIT $2",
        )
        .bind(store_id)
        .bind(limit)
        .fetch_all(pool)
        .await
        .unwrap_or_default()
    } else {
        sqlx::query_as::<_, AlertRow>(
            "SELECT id, store_id, camera_id, alert_type, confidence, message, is_read, created_at \
             FROM alerts \
             WHERE store_id = $1 \
             ORDER BY created_at DESC \
             LIMIT $2",
        )
        .bind(store_id)
        .bind(limit)
        .fetch_all(pool)
        .await
        .unwrap_or_default()
    }
}

/// Count unread alerts for a store.
pub async fn get_unread_alert_count(pool: &PgPool, store_id: &Uuid) -> i64 {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT COUNT(*)::bigint FROM alerts WHERE store_id = $1 AND is_read = false",
    )
    .bind(store_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    row.map(|r| r.0).unwrap_or(0)
}

/// Mark a single alert as read. Returns false if the alert was not found or not owned.
pub async fn mark_alert_read(
    pool: &PgPool,
    alert_id: &Uuid,
    store_id: &Uuid,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE alerts SET is_read = true WHERE id = $1 AND store_id = $2 AND is_read = false",
    )
    .bind(alert_id)
    .bind(store_id)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// Mark all alerts as read for a store. Returns the count of alerts marked.
pub async fn mark_all_alerts_read(
    pool: &PgPool,
    store_id: &Uuid,
) -> Result<i64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE alerts SET is_read = true WHERE store_id = $1 AND is_read = false",
    )
    .bind(store_id)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() as i64)
}
