use chrono::{NaiveDate, Utc};
use serde::Serialize;
use sqlx::postgres::PgPool;
use sqlx::FromRow;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Row structs (mapped from DB tables)
// ---------------------------------------------------------------------------

#[derive(Debug, FromRow)]
#[allow(dead_code)]
pub struct StoreRow {
    pub id: Uuid,
    pub owner_id: Uuid,
    pub name: String,
    pub plan_tier: String,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct CameraRow {
    pub id: Uuid,
    pub store_id: Uuid,
    pub name: String,
    pub status: String,
    pub last_seen_at: Option<chrono::DateTime<Utc>>,
}

#[derive(Debug, FromRow)]
#[allow(dead_code)]
pub struct DailyReportRow {
    pub store_id: Uuid,
    pub report_date: NaiveDate,
    pub total_visitors: i64,
    pub peak_hour: Option<i16>,
    pub demographics_summary: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Connection pool
// ---------------------------------------------------------------------------

pub async fn create_pool(database_url: &str) -> Result<PgPool, sqlx::Error> {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(database_url)
        .await
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// User queries
// ---------------------------------------------------------------------------

#[derive(Debug, FromRow)]
#[allow(dead_code)]
pub struct UserRow {
    pub id: Uuid,
    pub email: String,
    pub password_hash: String,
}

/// Find a user by email.
pub async fn find_user_by_email(pool: &PgPool, email: &str) -> Option<UserRow> {
    sqlx::query_as::<_, UserRow>(
        "SELECT id, email, password_hash FROM users WHERE email = $1 LIMIT 1",
    )
    .bind(email)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// Create a new user and return their ID.
pub async fn create_user(
    pool: &PgPool,
    email: &str,
    password_hash: &str,
) -> Result<Uuid, sqlx::Error> {
    let row: (Uuid,) =
        sqlx::query_as("INSERT INTO users (email, password_hash) VALUES ($1, $2) RETURNING id")
            .bind(email)
            .bind(password_hash)
            .fetch_one(pool)
            .await?;

    Ok(row.0)
}

/// Create a default store for a new user.
pub async fn create_default_store(
    pool: &PgPool,
    owner_id: &Uuid,
    store_name: &str,
) -> Result<Uuid, sqlx::Error> {
    let row: (Uuid,) =
        sqlx::query_as("INSERT INTO stores (owner_id, name) VALUES ($1, $2) RETURNING id")
            .bind(owner_id)
            .bind(store_name)
            .fetch_one(pool)
            .await?;

    Ok(row.0)
}

// ---------------------------------------------------------------------------
// Store queries
// ---------------------------------------------------------------------------

/// Fetch the first store owned by the given user.
pub async fn get_store_by_owner(pool: &PgPool, owner_id: &Uuid) -> Option<StoreRow> {
    sqlx::query_as::<_, StoreRow>(
        "SELECT id, owner_id, name, plan_tier FROM stores WHERE owner_id = $1 LIMIT 1",
    )
    .bind(owner_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// Fetch a store by its ID (regardless of owner).
pub async fn get_store_by_owner_or_id(pool: &PgPool, store_id: &Uuid) -> Option<StoreRow> {
    sqlx::query_as::<_, StoreRow>(
        "SELECT id, owner_id, name, plan_tier FROM stores WHERE id = $1 LIMIT 1",
    )
    .bind(store_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// Check if a user owns a specific store.
pub async fn user_owns_store(pool: &PgPool, owner_id: &Uuid, store_id: &Uuid) -> bool {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT 1 AS found FROM stores WHERE id = $1 AND owner_id = $2 LIMIT 1")
            .bind(store_id)
            .bind(owner_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();

    row.is_some()
}

/// Query today's aggregate stats from visitor_counts.
pub async fn get_store_stats_db(pool: &PgPool, store_id: &Uuid) -> (i64, i64) {
    // today_total: sum of people_count for today
    // cameras_online: count of cameras with status = 'online'
    let today = Utc::now().date_naive();
    let start = today.and_hms_opt(0, 0, 0).expect("valid midnight");

    let total_row: Option<(Option<i64>,)> = sqlx::query_as(
        "SELECT SUM(people_count)::bigint AS total \
         FROM visitor_counts \
         WHERE store_id = $1 AND counted_at >= $2::date::timestamptz",
    )
    .bind(store_id)
    .bind(start)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    let today_total = total_row.and_then(|r| r.0).unwrap_or(0);

    let cameras_row: Option<(i64,)> = sqlx::query_as(
        "SELECT COUNT(*)::bigint FROM cameras WHERE store_id = $1 AND status = 'online'",
    )
    .bind(store_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    let cameras_online = cameras_row.map(|r| r.0).unwrap_or(0);

    (today_total, cameras_online)
}

/// Query the most recent daily_report for a store (today or most recent).
pub async fn get_daily_report_db(pool: &PgPool, store_id: &Uuid) -> Option<DailyReportRow> {
    sqlx::query_as::<_, DailyReportRow>(
        "SELECT store_id, report_date, total_visitors, peak_hour, demographics_summary \
         FROM daily_reports \
         WHERE store_id = $1 \
         ORDER BY report_date DESC \
         LIMIT 1",
    )
    .bind(store_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// Insert a visitor_count record from a frame analysis.
pub async fn insert_visitor_count(
    pool: &PgPool,
    camera_id: &Uuid,
    people_count: i32,
    demographics_json: serde_json::Value,
    zones_json: serde_json::Value,
) -> Result<(), sqlx::Error> {
    // The DB trigger will auto-populate store_id from camera_id.
    sqlx::query(
        "INSERT INTO visitor_counts (camera_id, store_id, counted_at, people_count, demographics_json, zones_json) \
         SELECT $1, c.store_id, NOW(), $2, $3, $4 \
         FROM cameras c WHERE c.id = $1",
    )
    .bind(camera_id)
    .bind(people_count)
    .bind(&demographics_json)
    .bind(&zones_json)
    .execute(pool)
    .await?;

    Ok(())
}

/// Fetch the LINE user ID associated with a store (for LINE notification).
///
/// Returns `None` if the store has no linked LINE account or the column is NULL.
pub async fn get_store_line_user_id(pool: &PgPool, store_id: &Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT line_user_id FROM stores WHERE id = $1")
        .bind(store_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

/// Count cameras for a store (lightweight, just count).
pub async fn count_cameras(pool: &PgPool, store_id: &Uuid) -> i64 {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT COUNT(*)::bigint FROM cameras WHERE store_id = $1")
            .bind(store_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();

    row.map(|r| r.0).unwrap_or(0)
}

/// Fetch daily visitor totals for the past 7 days for a store.
///
/// Returns a vector of `(date, total_count)` tuples sorted by date ascending.
/// Days with no data are simply omitted from the result.
pub async fn get_weekly_visitor_counts(pool: &PgPool, store_id: &Uuid) -> Vec<(NaiveDate, i64)> {
    let rows: Vec<(NaiveDate, i64)> = sqlx::query_as(
        "SELECT counted_at::date AS day, SUM(people_count)::bigint AS total \
         FROM visitor_counts \
         WHERE store_id = $1 \
           AND counted_at >= (CURRENT_DATE - INTERVAL '6 days') \
         GROUP BY day \
         ORDER BY day",
    )
    .bind(store_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    rows
}

/// Fetch hourly visitor totals for today for a store.
///
/// Returns a vector of `(hour, total_count)` tuples sorted by hour ascending.
/// Hours with no data are simply omitted from the result.
pub async fn get_hourly_visitor_counts(pool: &PgPool, store_id: &Uuid) -> Vec<(i32, i64)> {
    let rows: Vec<(i32, i64)> = sqlx::query_as(
        "SELECT EXTRACT(HOUR FROM counted_at)::int AS hour, \
                SUM(people_count)::bigint AS total \
         FROM visitor_counts \
         WHERE store_id = $1 \
           AND counted_at >= CURRENT_DATE \
           AND counted_at < CURRENT_DATE + INTERVAL '1 day' \
         GROUP BY hour \
         ORDER BY hour",
    )
    .bind(store_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    rows
}

/// Look up a camera by its string name (or alias) within a store.
///
/// This handles the mismatch where agents use human-readable camera IDs
/// (e.g. "cam-1") but the DB uses UUIDs. Returns the camera UUID if found.
pub async fn find_camera_by_name(pool: &PgPool, store_id: &Uuid, name: &str) -> Option<Uuid> {
    let row: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM cameras WHERE store_id = $1 AND name = $2 LIMIT 1")
            .bind(store_id)
            .bind(name)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();

    row.map(|r| r.0)
}

/// Register a new camera for a store (used during pairing / first frame).
///
/// Returns the newly created camera UUID.
pub async fn register_camera(
    pool: &PgPool,
    store_id: &Uuid,
    name: &str,
) -> Result<Uuid, sqlx::Error> {
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO cameras (store_id, name, status) VALUES ($1, $2, 'online') RETURNING id",
    )
    .bind(store_id)
    .bind(name)
    .fetch_one(pool)
    .await?;

    Ok(row.0)
}

/// Consume a pairing code and return the associated store's token + info.
///
/// The `pairing_codes` table is ephemeral; codes expire after 10 minutes.
/// Returns `None` if the code is invalid or expired.
pub async fn consume_pairing_code(pool: &PgPool, code: &str) -> Option<(Uuid, String)> {
    // Try to find and consume the code in one atomic operation.
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "DELETE FROM pairing_codes \
         WHERE code = $1 AND expires_at > NOW() \
         RETURNING store_id, token",
    )
    .bind(code)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    row
}

/// Create a pairing code for a store. Returns the 6-digit code.
pub async fn create_pairing_code(
    pool: &PgPool,
    store_id: &Uuid,
    token: &str,
) -> Result<String, sqlx::Error> {
    use std::fmt::Write;
    // Generate a 6-digit numeric code.
    let mut code = String::new();
    let random_num: u32 = rand_u32() % 1_000_000;
    write!(&mut code, "{:06}", random_num).unwrap();

    sqlx::query(
        "INSERT INTO pairing_codes (code, store_id, token, expires_at) \
         VALUES ($1, $2, $3, NOW() + INTERVAL '10 minutes') \
         ON CONFLICT (code) DO UPDATE SET store_id = $2, token = $3, expires_at = NOW() + INTERVAL '10 minutes'",
    )
    .bind(&code)
    .bind(store_id)
    .bind(token)
    .execute(pool)
    .await?;

    Ok(code)
}

/// Simple deterministic random u32 seeded from current time (no external crate needed).
fn rand_u32() -> u32 {
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    // Simple hash to spread the value.
    ns.wrapping_mul(2654435761)
}

/// List cameras for a given store.
pub async fn get_cameras(pool: &PgPool, store_id: &Uuid) -> Vec<CameraRow> {
    sqlx::query_as::<_, CameraRow>(
        "SELECT id, store_id, name, status, last_seen_at \
         FROM cameras \
         WHERE store_id = $1 \
         ORDER BY name",
    )
    .bind(store_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}
