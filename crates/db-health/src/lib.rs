//! Camera health derivation shared between the API and tests.
//!
//! This crate intentionally has no database access: it only derives the
//! owner-facing camera state from the existing `cameras.status` /
//! `cameras.last_seen_at` columns, so no schema change is required.

use chrono::{DateTime, Utc};
use serde::Serialize;

/// Health of a camera as shown to the store owner.
///
/// Distinguishes three states (the `cameras.status` CHECK constraint already
/// permits 'online' | 'offline' | 'error'):
/// - `offline`: the agent has not sent a frame within the stale threshold
///   (camera/network/agent is disconnected). Derived from `last_seen_at`,
///   which overrides the stored status — the stored column alone cannot
///   express staleness.
/// - `error`: frames are arriving but recent AI analysis failed (the API
///   writes `status = 'error'` on analysis failure and `status = 'online'`
///   on success). Data flow is alive but results are unreliable.
/// - `online`: frames are flowing and analysis is succeeding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraHealth {
    Online,
    Offline,
    Error,
}

impl CameraHealth {
    pub fn as_str(&self) -> &'static str {
        match self {
            CameraHealth::Online => "online",
            CameraHealth::Offline => "offline",
            CameraHealth::Error => "error",
        }
    }
}

/// A camera is considered disconnected if no frame arrived within this window.
pub const CAMERA_OFFLINE_THRESHOLD_MINUTES: i64 = 5;

/// Compute the health of a camera from its stored status and last-seen
/// timestamp. `now` is injected for testability. A stale `last_seen_at`
/// always wins over the stored status; a fresh `last_seen_at` trusts the
/// stored status ('error' from the last analysis, else 'online').
pub fn compute_camera_health(
    stored_status: &str,
    last_seen_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> CameraHealth {
    match last_seen_at {
        None => CameraHealth::Offline,
        Some(ts) => {
            if now.signed_duration_since(ts).num_minutes() >= CAMERA_OFFLINE_THRESHOLD_MINUTES {
                CameraHealth::Offline
            } else if stored_status == "error" {
                CameraHealth::Error
            } else {
                CameraHealth::Online
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn health_offline_when_never_seen() {
        let now = Utc::now();
        assert_eq!(
            compute_camera_health("online", None, now),
            CameraHealth::Offline
        );
    }

    #[test]
    fn health_offline_when_stale() {
        let now = Utc::now();
        let stale = now - Duration::minutes(CAMERA_OFFLINE_THRESHOLD_MINUTES + 1);
        assert_eq!(
            compute_camera_health("online", Some(stale), now),
            CameraHealth::Offline
        );
        // Stale + stored error is still reported as offline (disconnection first).
        assert_eq!(
            compute_camera_health("error", Some(stale), now),
            CameraHealth::Offline
        );
    }

    #[test]
    fn health_online_when_recent_and_healthy() {
        let now = Utc::now();
        let recent = now - Duration::minutes(1);
        assert_eq!(
            compute_camera_health("online", Some(recent), now),
            CameraHealth::Online
        );
    }

    #[test]
    fn health_error_when_recent_but_failing() {
        let now = Utc::now();
        let recent = now - Duration::minutes(1);
        assert_eq!(
            compute_camera_health("error", Some(recent), now),
            CameraHealth::Error
        );
    }

    #[test]
    fn health_boundary_exactly_at_threshold_is_offline() {
        let now = Utc::now();
        let exact = now - Duration::minutes(CAMERA_OFFLINE_THRESHOLD_MINUTES);
        assert_eq!(
            compute_camera_health("online", Some(exact), now),
            CameraHealth::Offline
        );
    }
}
