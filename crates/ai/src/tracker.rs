/// Simple IoU-based centroid tracker for counting visitors and measuring dwell time.
///
/// Each detected bounding box is matched to an existing track using Intersection-over-Union.
/// Unmatched detections become new tracks; tracks unseen for MAX_MISSED frames are retired.
use std::collections::HashMap;
use std::time::Instant;

/// Normalised bounding box (0.0–1.0 relative to frame dimensions).
#[derive(Debug, Clone)]
pub struct BBox {
    pub x_min: f32,
    pub y_min: f32,
    pub x_max: f32,
    pub y_max: f32,
    pub confidence: f32,
}

impl BBox {
    pub fn iou(&self, other: &BBox) -> f32 {
        let ix1 = self.x_min.max(other.x_min);
        let iy1 = self.y_min.max(other.y_min);
        let ix2 = self.x_max.min(other.x_max);
        let iy2 = self.y_max.min(other.y_max);

        let iw = (ix2 - ix1).max(0.0);
        let ih = (iy2 - iy1).max(0.0);
        let inter = iw * ih;

        let area_a = (self.x_max - self.x_min) * (self.y_max - self.y_min);
        let area_b = (other.x_max - other.x_min) * (other.y_max - other.y_min);
        let union = area_a + area_b - inter;

        if union <= 0.0 {
            0.0
        } else {
            inter / union
        }
    }

    /// Centre point (normalised).
    pub fn center(&self) -> (f32, f32) {
        (
            (self.x_min + self.x_max) / 2.0,
            (self.y_min + self.y_max) / 2.0,
        )
    }
}

struct Track {
    bbox: BBox,
    first_seen: Instant,
    last_seen: Instant,
    missed: u32,
}

impl Track {
    fn dwell_secs(&self) -> f32 {
        self.last_seen.duration_since(self.first_seen).as_secs_f32()
    }
}

/// Output of a single `Tracker::update()` call.
#[derive(Debug, Clone)]
pub struct UpdateOutput {
    /// Number of people detected (active tracks this frame).
    pub people_count: u32,
    /// Tracks newly created this frame (new entrants).
    pub new_visitors: u32,
    /// Total unique visitors since tracker was created.
    pub total_unique: u32,
    /// Average dwell time of completed tracks (seconds).
    pub avg_dwell_secs: f32,
}

/// Per-camera stateful tracker. Wrap in `Arc<Mutex<…>>` for concurrent access.
pub struct Tracker {
    tracks: HashMap<u32, Track>,
    next_id: u32,
    total_unique: u32,
    completed_dwells: Vec<f32>,
    iou_threshold: f32,
    /// How many consecutive frames a track can go unmatched before retirement.
    max_missed: u32,
}

impl Tracker {
    pub fn new() -> Self {
        Self {
            tracks: HashMap::new(),
            next_id: 0,
            total_unique: 0,
            completed_dwells: Vec::new(),
            iou_threshold: 0.3,
            max_missed: 5,
        }
    }

    /// Update tracker with detections from the latest frame.
    pub fn update(&mut self, detections: &[BBox]) -> UpdateOutput {
        let now = Instant::now();
        let track_ids: Vec<u32> = self.tracks.keys().cloned().collect();
        let nt = track_ids.len();
        let nd = detections.len();

        let mut track_matched = vec![false; nt];
        let mut det_matched = vec![false; nd];

        // Build IoU matrix and greedily match best pairs.
        loop {
            let mut best = self.iou_threshold;
            let mut best_ti = usize::MAX;
            let mut best_di = usize::MAX;

            for (ti, &tid) in track_ids.iter().enumerate() {
                if track_matched[ti] {
                    continue;
                }
                let track_bbox = &self.tracks[&tid].bbox;
                for (di, det) in detections.iter().enumerate() {
                    if det_matched[di] {
                        continue;
                    }
                    let iou = track_bbox.iou(det);
                    if iou > best {
                        best = iou;
                        best_ti = ti;
                        best_di = di;
                    }
                }
            }

            if best_ti == usize::MAX {
                break;
            }

            let tid = track_ids[best_ti];
            let track = self.tracks.get_mut(&tid).unwrap();
            track.bbox = detections[best_di].clone();
            track.last_seen = now;
            track.missed = 0;
            track_matched[best_ti] = true;
            det_matched[best_di] = true;
        }

        // Increment missed counter; retire tracks that have exceeded the limit.
        let mut to_retire = Vec::new();
        for (ti, &tid) in track_ids.iter().enumerate() {
            if !track_matched[ti] {
                let track = self.tracks.get_mut(&tid).unwrap();
                track.missed += 1;
                if track.missed > self.max_missed {
                    to_retire.push(tid);
                }
            }
        }
        for tid in to_retire {
            if let Some(track) = self.tracks.remove(&tid) {
                self.completed_dwells.push(track.dwell_secs());
                // Cap history at 2000 entries.
                if self.completed_dwells.len() > 2000 {
                    self.completed_dwells.drain(0..1000);
                }
            }
        }

        // Create new tracks for unmatched detections.
        let mut new_visitors = 0u32;
        for (di, &matched) in det_matched.iter().enumerate() {
            if !matched {
                let id = self.next_id;
                self.next_id += 1;
                self.total_unique += 1;
                new_visitors += 1;
                self.tracks.insert(
                    id,
                    Track {
                        bbox: detections[di].clone(),
                        first_seen: now,
                        last_seen: now,
                        missed: 0,
                    },
                );
            }
        }

        let avg_dwell = if self.completed_dwells.is_empty() {
            0.0
        } else {
            self.completed_dwells.iter().sum::<f32>() / self.completed_dwells.len() as f32
        };

        UpdateOutput {
            people_count: self.current_count(),
            new_visitors,
            total_unique: self.total_unique,
            avg_dwell_secs: avg_dwell,
        }
    }

    /// Number of active tracks (people currently in frame).
    pub fn current_count(&self) -> u32 {
        self.tracks
            .values()
            .filter(|track| track.missed == 0)
            .count() as u32
    }

    /// Average dwell time across completed tracks.
    pub fn avg_dwell_secs(&self) -> f32 {
        if self.completed_dwells.is_empty() {
            0.0
        } else {
            self.completed_dwells.iter().sum::<f32>() / self.completed_dwells.len() as f32
        }
    }
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}
