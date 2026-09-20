pub mod demographics;
pub mod tracker;

pub use tracker::{BBox, Tracker, UpdateOutput};

use chrono::Utc;
use image::imageops::FilterType;
use ndarray::Array4;
use ort::session::Session;
use shared::{AgeGroup, AnalysisResult, DemographicEstimate, GenderEstimate, ZoneHeatmap};
use std::sync::OnceLock;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Model singleton
// ---------------------------------------------------------------------------

static MODEL: OnceLock<Option<Session>> = OnceLock::new();

const INPUT_W: u32 = 640;
const INPUT_H: u32 = 640;
const NUM_CANDIDATES: usize = 8400;
const NUM_CLASSES: usize = 80;
const CONF_THRESHOLD: f32 = 0.25;
const NMS_IOU_THRESHOLD: f32 = 0.45;
const PERSON_CLASS: usize = 0;

fn model_path() -> String {
    std::env::var("MISEBAN_MODEL_PATH").unwrap_or_else(|_| "models/yolov8n.onnx".to_string())
}

/// Load the YOLOv8n ONNX model. Safe to call multiple times (no-op after first call).
pub fn init_model() -> Result<(), AiError> {
    let model = MODEL.get_or_init(|| {
        let path = model_path();
        if !std::path::Path::new(&path).exists() {
            eprintln!(
                "[ai] Model not found at '{path}'. Run `scripts/download-model.sh` or set \
                 MISEBAN_MODEL_PATH. Analysis is unavailable until the model is loaded."
            );
            return None;
        }
        match Session::builder()
            .and_then(|b| b.with_intra_threads(4))
            .and_then(|b| b.commit_from_file(&path))
        {
            Ok(s) => {
                eprintln!("[ai] YOLOv8n model loaded from '{path}'");
                Some(s)
            }
            Err(e) => {
                eprintln!("[ai] Failed to load model: {e}");
                None
            }
        }
    });
    if model.is_some() {
        Ok(())
    } else {
        Err(AiError::ModelLoad("model unavailable".into()))
    }
}

// ---------------------------------------------------------------------------
// Synchronous people detection (call via spawn_blocking from async context)
// ---------------------------------------------------------------------------

/// Detect people in a JPEG frame. Returns normalised bounding boxes.
///
/// Model, decode and inference failures are distinct from a successful empty result.
pub fn detect_people_checked(jpeg: &[u8]) -> Result<Vec<BBox>, AiError> {
    let session = MODEL
        .get()
        .and_then(|model| model.as_ref())
        .ok_or_else(|| AiError::ModelLoad("model unavailable".into()))?;
    let input = preprocess(jpeg).map_err(AiError::Inference)?;
    let inputs = ort::inputs![input].map_err(|e| AiError::Inference(e.to_string()))?;
    let outputs = session
        .run(inputs)
        .map_err(|e| AiError::Inference(e.to_string()))?;
    if outputs.len() == 0 {
        return Err(AiError::Inference("model returned no outputs".into()));
    }
    let tensor = outputs[0]
        .try_extract_tensor::<f32>()
        .map_err(|e| AiError::Inference(e.to_string()))?;
    decode_detections(tensor.view())
}

fn decode_detections(view: ndarray::ArrayViewD<'_, f32>) -> Result<Vec<BBox>, AiError> {
    let shape = view.shape();
    if shape.len() != 3
        || shape[0] != 1
        || shape[1] != (4 + NUM_CLASSES)
        || shape[2] != NUM_CANDIDATES
    {
        return Err(AiError::Inference(format!(
            "unexpected output shape: {shape:?}"
        )));
    }
    if view.iter().any(|value| !value.is_finite()) {
        return Err(AiError::Inference("non-finite model output".into()));
    }

    let mut candidates: Vec<BBox> = Vec::new();
    for i in 0..NUM_CANDIDATES {
        let score = view[[0, 4 + PERSON_CLASS, i]];
        if score < CONF_THRESHOLD {
            continue;
        }
        let cx = view[[0, 0, i]];
        let cy = view[[0, 1, i]];
        let w = view[[0, 2, i]];
        let h = view[[0, 3, i]];

        candidates.push(BBox {
            x_min: ((cx - w / 2.0) / INPUT_W as f32).clamp(0.0, 1.0),
            y_min: ((cy - h / 2.0) / INPUT_H as f32).clamp(0.0, 1.0),
            x_max: ((cx + w / 2.0) / INPUT_W as f32).clamp(0.0, 1.0),
            y_max: ((cy + h / 2.0) / INPUT_H as f32).clamp(0.0, 1.0),
            confidence: score,
        });
    }

    candidates.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(nms(candidates, NMS_IOU_THRESHOLD))
}

fn preprocess(jpeg: &[u8]) -> Result<Array4<f32>, String> {
    let img = image::load_from_memory(jpeg).map_err(|e| e.to_string())?;
    let rgb = img
        .resize_exact(INPUT_W, INPUT_H, FilterType::Triangle)
        .to_rgb8();

    let h = INPUT_H as usize;
    let w = INPUT_W as usize;
    let mut data = vec![0.0f32; 3 * h * w];
    let hw = h * w;

    for (i, p) in rgb.pixels().enumerate() {
        data[i] = p[0] as f32 / 255.0;
        data[hw + i] = p[1] as f32 / 255.0;
        data[2 * hw + i] = p[2] as f32 / 255.0;
    }

    Array4::from_shape_vec((1, 3, h, w), data).map_err(|e| e.to_string())
}

fn iou(a: &BBox, b: &BBox) -> f32 {
    a.iou(b)
}

fn nms(candidates: Vec<BBox>, threshold: f32) -> Vec<BBox> {
    let mut kept = Vec::new();
    let mut suppressed = vec![false; candidates.len()];

    for i in 0..candidates.len() {
        if suppressed[i] {
            continue;
        }
        kept.push(candidates[i].clone());
        for j in (i + 1)..candidates.len() {
            if !suppressed[j] && iou(&candidates[i], &candidates[j]) > threshold {
                suppressed[j] = true;
            }
        }
    }
    kept
}

// ---------------------------------------------------------------------------
// Zone computation
// ---------------------------------------------------------------------------

/// Map detections to a simple 3-zone layout: entrance (left), center, register (right).
pub fn compute_zones(detections: &[BBox]) -> Vec<ZoneHeatmap> {
    if detections.is_empty() {
        return vec![];
    }

    let mut entrance = 0u32;
    let mut center = 0u32;
    let mut register = 0u32;

    for bbox in detections {
        let (cx, _) = bbox.center();
        if cx < 0.33 {
            entrance += 1;
        } else if cx < 0.67 {
            center += 1;
        } else {
            register += 1;
        }
    }

    let mut zones = Vec::new();
    if entrance > 0 {
        zones.push(ZoneHeatmap {
            zone_name: "entrance".into(),
            x_min: 0.0,
            y_min: 0.0,
            x_max: 0.33,
            y_max: 1.0,
            count: entrance,
        });
    }
    if center > 0 {
        zones.push(ZoneHeatmap {
            zone_name: "center".into(),
            x_min: 0.33,
            y_min: 0.0,
            x_max: 0.67,
            y_max: 1.0,
            count: center,
        });
    }
    if register > 0 {
        zones.push(ZoneHeatmap {
            zone_name: "register".into(),
            x_min: 0.67,
            y_min: 0.0,
            x_max: 1.0,
            y_max: 1.0,
            count: register,
        });
    }
    zones
}

// ---------------------------------------------------------------------------
// High-level frame analysis
// ---------------------------------------------------------------------------

/// Analyze a single frame synchronously (no demographics, no tracking).
/// Callers that need demographics + tracking should call the lower-level functions
/// directly: `detect_people_checked`, `Tracker::update`, `demographics::estimate`.
pub async fn analyze_frame_checked(frame: &shared::FrameData) -> Result<AnalysisResult, AiError> {
    let jpeg = frame.jpeg_bytes.clone();
    let detections = tokio::task::spawn_blocking(move || detect_people_checked(&jpeg))
        .await
        .map_err(|e| AiError::Inference(format!("analysis task failed: {e}")))??;

    let people_count = detections.len() as u32;
    let zones = compute_zones(&detections);

    let demographics: Vec<DemographicEstimate> = (0..people_count)
        .map(|_| DemographicEstimate {
            age_group: AgeGroup::Adult,
            gender: GenderEstimate::Unknown,
            confidence: 0.30,
        })
        .collect();

    Ok(AnalysisResult {
        id: Uuid::new_v4(),
        camera_id: frame.camera_id.clone(),
        timestamp: Utc::now(),
        people_count,
        demographics,
        zones,
        alerts: vec![],
        avg_dwell_secs: 0.0,
        unique_visitors: people_count,
    })
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum AiError {
    ModelLoad(String),
    Inference(String),
}

impl std::fmt::Display for AiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AiError::ModelLoad(m) => write!(f, "model load error: {m}"),
            AiError::Inference(m) => write!(f, "inference error: {m}"),
        }
    }
}

impl std::error::Error for AiError {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use shared::{FrameData, Resolution};

    #[tokio::test]
    async fn test_analyze_frame_no_model() {
        // A missing model must never be reported as zero visitors.
        let frame = FrameData {
            camera_id: "cam-01".to_string(),
            timestamp: chrono::Utc::now(),
            jpeg_bytes: vec![0xFF, 0xD8, 0xFF, 0xE0], // minimal JPEG header
            resolution: Resolution {
                width: 640,
                height: 480,
            },
        };
        assert!(matches!(
            analyze_frame_checked(&frame).await,
            Err(AiError::ModelLoad(_))
        ));
    }

    #[test]
    fn valid_empty_output_is_zero_but_invalid_output_is_an_error() {
        let empty = ndarray::Array3::<f32>::zeros((1, 4 + NUM_CLASSES, NUM_CANDIDATES));
        assert!(decode_detections(empty.view().into_dyn())
            .unwrap()
            .is_empty());
        let invalid = ndarray::Array3::<f32>::zeros((0, 4 + NUM_CLASSES, NUM_CANDIDATES));
        assert!(decode_detections(invalid.view().into_dyn()).is_err());
        let mut invalid = empty;
        invalid[[0, 4, 0]] = f32::NAN;
        assert!(decode_detections(invalid.view().into_dyn()).is_err());
        assert!(preprocess(b"broken JPEG").is_err());
    }

    #[test]
    fn test_tracker_basic() {
        let mut tracker = Tracker::new();
        let dets = vec![BBox {
            x_min: 0.1,
            y_min: 0.1,
            x_max: 0.3,
            y_max: 0.8,
            confidence: 0.9,
        }];
        let out = tracker.update(&dets);
        assert_eq!(out.people_count, 1);
        assert_eq!(out.new_visitors, 1);
        assert_eq!(out.total_unique, 1);

        // Same position next frame — track continues.
        let out2 = tracker.update(&dets);
        assert_eq!(out2.people_count, 1);
        assert_eq!(out2.new_visitors, 0); // not new
        assert_eq!(out2.total_unique, 1);
    }
}
