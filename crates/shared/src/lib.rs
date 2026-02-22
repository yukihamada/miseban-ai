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
