use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use sqlx::PgPool;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::db;

// ---------------------------------------------------------------------------
// Rate limiter (in-memory, per-store)
// ---------------------------------------------------------------------------

/// In-memory rate limiter: tracks last frame submission time per store.
#[derive(Clone)]
pub struct RateLimiter {
    state: Arc<Mutex<HashMap<Uuid, Instant>>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Check if a store can submit a frame based on its tier's rate limit.
    /// Returns true if allowed, false if rate limited.
    pub async fn check(&self, store_id: &Uuid, tier: &str) -> bool {
        let min_interval = min_interval_for_tier(tier);
        let mut map = self.state.lock().await;

        if let Some(last) = map.get(store_id) {
            if last.elapsed() < min_interval {
                return false;
            }
        }

        map.insert(*store_id, Instant::now());
        true
    }
}

/// Minimum interval between frame submissions per tier.
fn min_interval_for_tier(tier: &str) -> std::time::Duration {
    match tier {
        "starter" => std::time::Duration::from_secs(2),     // 0.5 fps
        "pro" => std::time::Duration::from_millis(500),      // 2 fps
        "enterprise" => std::time::Duration::from_millis(100), // 10 fps
        _ => std::time::Duration::from_secs(10),             // free: 1 frame / 10s
    }
}

// ---------------------------------------------------------------------------
// Plan tier enforcement
// ---------------------------------------------------------------------------

/// Check if a store can add more cameras based on its plan tier.
pub async fn can_add_camera(pool: &PgPool, store_id: &Uuid) -> Result<bool, sqlx::Error> {
    let tier = get_plan_tier(pool, store_id).await?;
    let max = max_cameras_for_tier(&tier);

    let count = db::count_cameras(pool, store_id).await;
    Ok((count as usize) < max)
}

/// Get the plan tier string for a store.
pub async fn get_plan_tier(pool: &PgPool, store_id: &Uuid) -> Result<String, sqlx::Error> {
    let tier: Option<String> =
        sqlx::query_scalar("SELECT plan_tier FROM stores WHERE id = $1")
            .bind(store_id)
            .fetch_optional(pool)
            .await?;

    Ok(tier.unwrap_or_else(|| "free".to_string()))
}

/// Maximum cameras allowed per plan tier.
pub fn max_cameras_for_tier(tier: &str) -> usize {
    match tier {
        "starter" => 4,
        "pro" => 16,
        "enterprise" => usize::MAX,
        _ => 1, // free
    }
}

/// Maximum data retention days per plan tier.
pub fn retention_days_for_tier(tier: &str) -> i64 {
    match tier {
        "starter" => 30,
        "pro" => 90,
        "enterprise" => 365 * 10, // effectively unlimited
        _ => 7,                   // free
    }
}

/// Check if a specific feature is available for a plan tier.
pub fn tier_has_feature(tier: &str, feature: &str) -> bool {
    match feature {
        "basic_count" => true, // all tiers
        "demographics" => tier != "free",
        "heatmaps" => matches!(tier, "pro" | "enterprise"),
        "alerts" => matches!(tier, "pro" | "enterprise"),
        "line_alerts" => tier != "free",
        "csv_export" => matches!(tier, "pro" | "enterprise"),
        "api_access" => matches!(tier, "pro" | "enterprise"),
        "custom_models" => tier == "enterprise",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_max_cameras() {
        assert_eq!(max_cameras_for_tier("free"), 1);
        assert_eq!(max_cameras_for_tier("starter"), 4);
        assert_eq!(max_cameras_for_tier("pro"), 16);
        assert_eq!(max_cameras_for_tier("enterprise"), usize::MAX);
        assert_eq!(max_cameras_for_tier("unknown"), 1);
    }

    #[test]
    fn test_retention_days() {
        assert_eq!(retention_days_for_tier("free"), 7);
        assert_eq!(retention_days_for_tier("starter"), 30);
        assert_eq!(retention_days_for_tier("pro"), 90);
        assert_eq!(retention_days_for_tier("enterprise"), 3650);
    }

    #[test]
    fn test_tier_features() {
        // Free tier
        assert!(tier_has_feature("free", "basic_count"));
        assert!(!tier_has_feature("free", "demographics"));
        assert!(!tier_has_feature("free", "heatmaps"));
        assert!(!tier_has_feature("free", "alerts"));
        assert!(!tier_has_feature("free", "line_alerts"));

        // Starter tier
        assert!(tier_has_feature("starter", "demographics"));
        assert!(tier_has_feature("starter", "line_alerts"));
        assert!(!tier_has_feature("starter", "heatmaps"));

        // Pro tier
        assert!(tier_has_feature("pro", "heatmaps"));
        assert!(tier_has_feature("pro", "alerts"));
        assert!(tier_has_feature("pro", "csv_export"));
        assert!(tier_has_feature("pro", "api_access"));
        assert!(!tier_has_feature("pro", "custom_models"));

        // Enterprise tier
        assert!(tier_has_feature("enterprise", "custom_models"));
    }

    #[tokio::test]
    async fn test_rate_limiter_allows_first_request() {
        let limiter = RateLimiter::new();
        let store_id = Uuid::new_v4();
        assert!(limiter.check(&store_id, "free").await);
    }

    #[tokio::test]
    async fn test_rate_limiter_blocks_rapid_requests() {
        let limiter = RateLimiter::new();
        let store_id = Uuid::new_v4();
        // First request: allowed
        assert!(limiter.check(&store_id, "free").await);
        // Immediate second request: blocked (free tier = 10s interval)
        assert!(!limiter.check(&store_id, "free").await);
    }

    #[tokio::test]
    async fn test_rate_limiter_different_stores_independent() {
        let limiter = RateLimiter::new();
        let store_a = Uuid::new_v4();
        let store_b = Uuid::new_v4();
        assert!(limiter.check(&store_a, "free").await);
        // Different store should be allowed even after store_a submitted
        assert!(limiter.check(&store_b, "free").await);
    }
}
