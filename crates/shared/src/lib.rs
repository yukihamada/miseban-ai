use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Frame & Camera
// ---------------------------------------------------------------------------

/// A single frame captured from a camera.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameData {
    pub camera_id: String,
    pub timestamp: DateTime<Utc>,
    /// JPEG-encoded image bytes (base64-encoded when serialized over JSON).
    #[serde(with = "base64_bytes")]
    pub jpeg_bytes: Vec<u8>,
    pub resolution: Resolution,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

/// Configuration for a single camera.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraConfig {
    pub id: String,
    pub name: String,
    pub rtsp_url: String,
    /// How many seconds between sampled frames (e.g. 5 = 1 frame every 5 s).
    pub fps_sample_rate: u64,
}

// ---------------------------------------------------------------------------
// Store & Plan
// ---------------------------------------------------------------------------

/// Top-level configuration for a store (shop / location).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreConfig {
    pub store_id: String,
    pub store_name: String,
    pub cameras: Vec<CameraConfig>,
    pub plan_tier: PlanTier,
}

/// Subscription plan tiers with associated limits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PlanTier {
    /// 1 camera, basic people-count only, 7-day retention.
    Free,
    /// Up to 4 cameras, demographics, 30-day retention.
    Starter,
    /// Up to 16 cameras, heatmaps + alerts, 90-day retention.
    Pro,
    /// Unlimited cameras, custom models, unlimited retention.
    Enterprise,
}

impl PlanTier {
    /// Maximum number of cameras allowed for this tier.
    pub fn max_cameras(&self) -> usize {
        match self {
            PlanTier::Free => 1,
            PlanTier::Starter => 4,
            PlanTier::Pro => 16,
            PlanTier::Enterprise => usize::MAX,
        }
    }

    /// Data retention in days.
    pub fn retention_days(&self) -> u32 {
        match self {
            PlanTier::Free => 7,
            PlanTier::Starter => 30,
            PlanTier::Pro => 90,
            PlanTier::Enterprise => u32::MAX, // unlimited
        }
    }
}

// ---------------------------------------------------------------------------
// Analysis results
// ---------------------------------------------------------------------------

/// Result of analyzing a single frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisResult {
    pub id: Uuid,
    pub camera_id: String,
    pub timestamp: DateTime<Utc>,
    pub people_count: u32,
    pub demographics: Vec<DemographicEstimate>,
    pub zones: Vec<ZoneHeatmap>,
    pub alerts: Vec<Alert>,
    /// Average dwell time in seconds across completed tracks (0.0 if no tracking data yet).
    #[serde(default)]
    pub avg_dwell_secs: f32,
    /// Cumulative unique visitors tracked since the agent session started.
    #[serde(default)]
    pub unique_visitors: u32,
}

/// Estimated demographics for a detected person.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemographicEstimate {
    pub age_group: AgeGroup,
    pub gender: GenderEstimate,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgeGroup {
    Child,
    Teen,
    YoungAdult,
    Adult,
    Senior,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum GenderEstimate {
    Male,
    Female,
    Unknown,
}

/// Heatmap data for a named zone within the camera frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneHeatmap {
    pub zone_name: String,
    /// Normalised coordinates (0.0 - 1.0).
    pub x_min: f32,
    pub y_min: f32,
    pub x_max: f32,
    pub y_max: f32,
    /// Number of people detected within the zone.
    pub count: u32,
}

// ---------------------------------------------------------------------------
// Alerts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    pub alert_type: AlertType,
    pub timestamp: DateTime<Utc>,
    pub camera_id: String,
    /// Detection confidence 0.0 - 1.0.
    pub confidence: f32,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AlertType {
    /// Someone entered a restricted zone.
    Intrusion,
    /// Unusual behaviour detected (loitering, running, etc.).
    Unusual,
    /// Crowd density exceeds threshold.
    Crowding,
}

// ---------------------------------------------------------------------------
// Daily report
// ---------------------------------------------------------------------------

/// Aggregated statistics for one calendar day.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyReport {
    pub store_id: String,
    pub date: NaiveDate,
    pub total_visitors: u64,
    /// Hour of the day with the most visitors (0-23).
    pub peak_hour: u8,
    pub demographics_summary: DemographicsSummary,
}

/// Summary of demographics across a time period.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemographicsSummary {
    pub age_distribution: Vec<AgeDistribution>,
    pub gender_distribution: Vec<GenderDistribution>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgeDistribution {
    pub age_group: AgeGroup,
    pub percentage: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenderDistribution {
    pub gender: GenderEstimate,
    pub percentage: f32,
}

// ---------------------------------------------------------------------------
// Base64 serde helper for jpeg_bytes
// ---------------------------------------------------------------------------

mod base64_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        serializer.serialize_str(&encoded)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        use base64::Engine;
        let s = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(s)
            .map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Tests — core domain invariants (no network, no I/O).
// These guard the billing boundary (plan limits) and the agent→API wire
// format (frame (de)serialization). If they break, customers get the wrong
// camera limits/retention, or frames silently fail to parse.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Plan tiers map to the exact camera limits and retention the pricing page
    /// promises. A regression here = over-provisioning or wrongly blocking a
    /// paying customer.
    #[test]
    fn plan_limits_match_pricing() {
        assert_eq!(PlanTier::Free.max_cameras(), 1);
        assert_eq!(PlanTier::Starter.max_cameras(), 4);
        assert_eq!(PlanTier::Pro.max_cameras(), 16);
        assert_eq!(PlanTier::Enterprise.max_cameras(), usize::MAX);

        assert_eq!(PlanTier::Free.retention_days(), 7);
        assert_eq!(PlanTier::Starter.retention_days(), 30);
        assert_eq!(PlanTier::Pro.retention_days(), 90);
        assert_eq!(PlanTier::Enterprise.retention_days(), u32::MAX);

        // Higher tiers must never offer fewer cameras / less retention.
        assert!(PlanTier::Free.max_cameras() <= PlanTier::Starter.max_cameras());
        assert!(PlanTier::Starter.max_cameras() <= PlanTier::Pro.max_cameras());
        assert!(PlanTier::Pro.max_cameras() <= PlanTier::Enterprise.max_cameras());
    }

    /// A FrameData with raw JPEG bytes survives a JSON round-trip (base64) and
    /// comes back byte-identical. This is the agent→API wire contract.
    #[test]
    fn frame_data_json_round_trips() {
        let frame = FrameData {
            camera_id: "cam-01".to_string(),
            timestamp: Utc::now(),
            jpeg_bytes: vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46],
            resolution: Resolution {
                width: 1920,
                height: 1080,
            },
        };
        let json = serde_json::to_string(&frame).expect("serialize");
        let back: FrameData = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.camera_id, frame.camera_id);
        assert_eq!(back.jpeg_bytes, frame.jpeg_bytes);
        assert_eq!(back.resolution.width, 1920);
        assert_eq!(back.resolution.height, 1080);
    }

    /// Malformed input must return an Err, never panic. A single bad frame
    /// from a flaky agent must not take down the request handler.
    #[test]
    fn malformed_frame_json_errors_not_panics() {
        // Invalid base64 in jpeg_bytes.
        let bad_b64 = r#"{"camera_id":"c","timestamp":"2026-01-01T00:00:00Z","jpeg_bytes":"!!!not-base64!!!","resolution":{"width":1,"height":1}}"#;
        assert!(serde_json::from_str::<FrameData>(bad_b64).is_err());

        // Missing required field (resolution).
        let missing = r#"{"camera_id":"c","timestamp":"2026-01-01T00:00:00Z","jpeg_bytes":"AAAA"}"#;
        assert!(serde_json::from_str::<FrameData>(missing).is_err());

        // Completely unrelated JSON.
        assert!(serde_json::from_str::<FrameData>("[]").is_err());
    }
}
