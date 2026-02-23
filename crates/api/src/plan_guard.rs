use sqlx::PgPool;
use uuid::Uuid;

use crate::db;

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

/// Check if a camera belongs to a store that can still accept frames.
/// This is a lightweight check called on every frame submission.
///
/// For now, all tiers can submit frames.
/// Future: rate limit based on tier.
///   Free: 1 frame/10s, Starter: 1 frame/5s, Pro: 1 frame/1s
#[allow(dead_code)]
pub async fn can_submit_frame(_pool: &PgPool, _store_id: &Uuid) -> bool {
    true
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
}
