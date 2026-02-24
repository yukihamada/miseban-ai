use chrono::Utc;
use shared::{
    AgeGroup, Alert, AnalysisResult, DemographicEstimate, FrameData, GenderEstimate, ZoneHeatmap,
};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Mock implementation (enabled via `cargo build --features mock`)
// ---------------------------------------------------------------------------

#[cfg(feature = "mock")]
pub fn init_model() -> Result<(), AiError> {
    Ok(())
}

#[cfg(feature = "mock")]
pub async fn analyze_frame(frame: &FrameData) -> AnalysisResult {
    let now = Utc::now();

    let demographics = vec![
        DemographicEstimate {
            age_group: AgeGroup::YoungAdult,
            gender: GenderEstimate::Female,
            confidence: 0.87,
        },
        DemographicEstimate {
            age_group: AgeGroup::Adult,
            gender: GenderEstimate::Male,
            confidence: 0.92,
        },
        DemographicEstimate {
            age_group: AgeGroup::Senior,
            gender: GenderEstimate::Male,
            confidence: 0.78,
        },
    ];

    let zones = vec![
        ZoneHeatmap {
            zone_name: "entrance".to_string(),
            x_min: 0.0,
            y_min: 0.0,
            x_max: 0.3,
            y_max: 1.0,
            count: 1,
        },
        ZoneHeatmap {
            zone_name: "register".to_string(),
            x_min: 0.7,
            y_min: 0.0,
            x_max: 1.0,
            y_max: 0.5,
            count: 2,
        },
    ];

    let alerts: Vec<Alert> = vec![];

    AnalysisResult {
        id: Uuid::new_v4(),
        camera_id: frame.camera_id.clone(),
        timestamp: now,
        people_count: demographics.len() as u32,
        demographics,
        zones,
        alerts,
    }
}

// ---------------------------------------------------------------------------
// Real inference implementation
// ---------------------------------------------------------------------------

#[cfg(not(feature = "mock"))]
mod inference {
    use image::imageops::FilterType;
    use ndarray::Array4;
    use ort::session::Session;
    use std::sync::OnceLock;

    use super::AiError;

    /// Global model singleton -- loaded once via `init_model()`.
    static MODEL: OnceLock<Session> = OnceLock::new();

    /// YOLOv8 input dimensions.
    const INPUT_W: u32 = 640;
    const INPUT_H: u32 = 640;

    /// Number of detection candidates in YOLOv8 output.
    const NUM_CANDIDATES: usize = 8400;

    /// COCO class count used by YOLOv8.
    const NUM_CLASSES: usize = 80;

    /// Confidence threshold for a detection to be considered.
    const CONF_THRESHOLD: f32 = 0.25;

    /// IoU threshold for Non-Maximum Suppression.
    const NMS_IOU_THRESHOLD: f32 = 0.45;

    /// COCO class index for "person".
    const PERSON_CLASS: usize = 0;

    /// A single bounding-box detection after post-processing.
    #[derive(Debug, Clone)]
    pub struct Detection {
        /// Normalised x-min (0..1 relative to the 640x640 input).
        pub x_min: f32,
        /// Normalised y-min.
        pub y_min: f32,
        /// Normalised x-max.
        pub x_max: f32,
        /// Normalised y-max.
        pub y_max: f32,
        /// Confidence score.
        pub confidence: f32,
    }

    /// Resolve the model file path from environment or default.
    fn model_path() -> String {
        std::env::var("MISEBAN_MODEL_PATH").unwrap_or_else(|_| "models/yolov8n.onnx".to_string())
    }

    /// Load the YOLOv8n ONNX model into the global singleton.
    ///
    /// Safe to call multiple times -- subsequent calls are no-ops.
    pub fn load_model() -> Result<(), AiError> {
        if MODEL.get().is_some() {
            return Ok(());
        }

        let path = model_path();
        let session = Session::builder()
            .map_err(|e| AiError::ModelLoad(format!("failed to create session builder: {e}")))?
            .with_intra_threads(4)
            .map_err(|e| AiError::ModelLoad(format!("failed to set intra threads: {e}")))?
            .commit_from_file(&path)
            .map_err(|e| AiError::ModelLoad(format!("failed to load model from {path}: {e}")))?;

        // Ignore error if another thread beat us to it.
        let _ = MODEL.set(session);
        Ok(())
    }

    /// Get a reference to the loaded model session.
    fn session() -> Result<&'static Session, AiError> {
        MODEL.get().ok_or(AiError::ModelNotLoaded)
    }

    /// Decode JPEG bytes, resize to 640x640, and produce an NCHW f32 tensor.
    ///
    /// Returns an `Array4<f32>` of shape `[1, 3, 640, 640]` with pixel values
    /// normalised to `[0.0, 1.0]`.
    fn preprocess(jpeg_bytes: &[u8]) -> Result<Array4<f32>, AiError> {
        let img = image::load_from_memory(jpeg_bytes)
            .map_err(|e| AiError::Preprocess(format!("image decode failed: {e}")))?;

        let resized = img.resize_exact(INPUT_W, INPUT_H, FilterType::Triangle);
        let rgb = resized.to_rgb8();

        // Build NCHW tensor: [1, 3, H, W]
        let h = INPUT_H as usize;
        let w = INPUT_W as usize;
        let mut tensor_data = vec![0.0f32; 3 * h * w];
        let hw = h * w;

        for (i, pixel) in rgb.pixels().enumerate() {
            tensor_data[i] = pixel[0] as f32 / 255.0; // R channel
            tensor_data[hw + i] = pixel[1] as f32 / 255.0; // G channel
            tensor_data[2 * hw + i] = pixel[2] as f32 / 255.0; // B channel
        }

        let array = Array4::from_shape_vec((1, 3, h, w), tensor_data)
            .map_err(|e| AiError::Preprocess(format!("failed to create ndarray: {e}")))?;

        Ok(array)
    }

    /// Run the model and return person detections.
    ///
    /// The returned coordinates are normalised to `[0, 1]` relative to the
    /// 640x640 input (the caller should remap if the original frame has a
    /// different aspect ratio).
    pub fn detect_people(jpeg_bytes: &[u8]) -> Result<Vec<Detection>, AiError> {
        let session = session()?;
        let input = preprocess(jpeg_bytes)?;

        let outputs = session
            .run(
                ort::inputs![input]
                    .map_err(|e| AiError::Inference(format!("failed to create inputs: {e}")))?,
            )
            .map_err(|e| AiError::Inference(format!("inference failed: {e}")))?;

        // YOLOv8 output shape: [1, 84, 8400]
        //   - 84 = 4 (cx, cy, w, h) + 80 (class scores)
        //   - 8400 detection candidates
        let output_tensor = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| AiError::Postprocess(format!("failed to extract output tensor: {e}")))?;

        let output_view = output_tensor.view();
        let shape = output_view.shape();

        // Validate shape
        if shape.len() != 3 || shape[1] != (4 + NUM_CLASSES) || shape[2] != NUM_CANDIDATES {
            return Err(AiError::Postprocess(format!(
                "unexpected output shape: {:?}, expected [1, {}, {}]",
                shape,
                4 + NUM_CLASSES,
                NUM_CANDIDATES,
            )));
        }

        let mut candidates: Vec<Detection> = Vec::new();

        for i in 0..NUM_CANDIDATES {
            // Extract class score for "person" (class 0).
            let person_score = output_view[[0, 4 + PERSON_CLASS, i]];

            if person_score < CONF_THRESHOLD {
                continue;
            }

            // Extract bounding box (cx, cy, w, h) in pixel coords (640x640).
            let cx = output_view[[0, 0, i]];
            let cy = output_view[[0, 1, i]];
            let w = output_view[[0, 2, i]];
            let h = output_view[[0, 3, i]];

            // Convert to normalised (x_min, y_min, x_max, y_max).
            let x_min = ((cx - w / 2.0) / INPUT_W as f32).clamp(0.0, 1.0);
            let y_min = ((cy - h / 2.0) / INPUT_H as f32).clamp(0.0, 1.0);
            let x_max = ((cx + w / 2.0) / INPUT_W as f32).clamp(0.0, 1.0);
            let y_max = ((cy + h / 2.0) / INPUT_H as f32).clamp(0.0, 1.0);

            candidates.push(Detection {
                x_min,
                y_min,
                x_max,
                y_max,
                confidence: person_score,
            });
        }

        // Sort by confidence descending for NMS.
        candidates.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Non-Maximum Suppression.
        let detections = nms(candidates, NMS_IOU_THRESHOLD);

        Ok(detections)
    }

    /// Compute Intersection-over-Union between two bounding boxes.
    fn iou(a: &Detection, b: &Detection) -> f32 {
        let inter_x_min = a.x_min.max(b.x_min);
        let inter_y_min = a.y_min.max(b.y_min);
        let inter_x_max = a.x_max.min(b.x_max);
        let inter_y_max = a.y_max.min(b.y_max);

        let inter_w = (inter_x_max - inter_x_min).max(0.0);
        let inter_h = (inter_y_max - inter_y_min).max(0.0);
        let inter_area = inter_w * inter_h;

        let area_a = (a.x_max - a.x_min) * (a.y_max - a.y_min);
        let area_b = (b.x_max - b.x_min) * (b.y_max - b.y_min);
        let union_area = area_a + area_b - inter_area;

        if union_area <= 0.0 {
            0.0
        } else {
            inter_area / union_area
        }
    }

    /// Greedy Non-Maximum Suppression.
    fn nms(candidates: Vec<Detection>, iou_threshold: f32) -> Vec<Detection> {
        let mut kept: Vec<Detection> = Vec::new();
        let mut suppressed = vec![false; candidates.len()];

        for i in 0..candidates.len() {
            if suppressed[i] {
                continue;
            }
            kept.push(candidates[i].clone());
            for j in (i + 1)..candidates.len() {
                if suppressed[j] {
                    continue;
                }
                if iou(&candidates[i], &candidates[j]) > iou_threshold {
                    suppressed[j] = true;
                }
            }
        }

        kept
    }
}

#[cfg(not(feature = "mock"))]
pub fn init_model() -> Result<(), AiError> {
    inference::load_model()
}

#[cfg(not(feature = "mock"))]
pub async fn analyze_frame(frame: &FrameData) -> AnalysisResult {
    let now = Utc::now();

    let detections = match inference::detect_people(&frame.jpeg_bytes) {
        Ok(dets) => dets,
        Err(e) => {
            eprintln!("[ai] inference error: {e}");
            // Return empty result on error instead of panicking.
            return AnalysisResult {
                id: Uuid::new_v4(),
                camera_id: frame.camera_id.clone(),
                timestamp: now,
                people_count: 0,
                demographics: vec![],
                zones: vec![],
                alerts: vec![],
            };
        }
    };

    let people_count = detections.len() as u32;

    // Stub demographics: each detected person gets an "Unknown" placeholder
    // until a dedicated demographics model is integrated.
    let demographics: Vec<DemographicEstimate> = detections
        .iter()
        .map(|d| DemographicEstimate {
            age_group: AgeGroup::Adult,
            gender: GenderEstimate::Unknown,
            confidence: d.confidence,
        })
        .collect();

    // Map detections into a simple full-frame zone for now.
    // A proper zone system would use store-specific zone configs.
    let zones = if people_count > 0 {
        vec![ZoneHeatmap {
            zone_name: "full_frame".to_string(),
            x_min: 0.0,
            y_min: 0.0,
            x_max: 1.0,
            y_max: 1.0,
            count: people_count,
        }]
    } else {
        vec![]
    };

    let alerts: Vec<Alert> = vec![];

    AnalysisResult {
        id: Uuid::new_v4(),
        camera_id: frame.camera_id.clone(),
        timestamp: now,
        people_count,
        demographics,
        zones,
        alerts,
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur in the AI inference pipeline.
#[derive(Debug)]
pub enum AiError {
    /// Failed to load the ONNX model.
    ModelLoad(String),
    /// Model has not been initialized -- call `init_model()` first.
    ModelNotLoaded,
    /// Image preprocessing failed (decode, resize, etc.).
    #[cfg(not(feature = "mock"))]
    Preprocess(String),
    /// ONNX runtime inference failed.
    #[cfg(not(feature = "mock"))]
    Inference(String),
    /// Output post-processing failed (unexpected shape, etc.).
    #[cfg(not(feature = "mock"))]
    Postprocess(String),
}

impl std::fmt::Display for AiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AiError::ModelLoad(msg) => write!(f, "model load error: {msg}"),
            AiError::ModelNotLoaded => write!(f, "model not loaded -- call init_model() first"),
            #[cfg(not(feature = "mock"))]
            AiError::Preprocess(msg) => write!(f, "preprocess error: {msg}"),
            #[cfg(not(feature = "mock"))]
            AiError::Inference(msg) => write!(f, "inference error: {msg}"),
            #[cfg(not(feature = "mock"))]
            AiError::Postprocess(msg) => write!(f, "postprocess error: {msg}"),
        }
    }
}

impl std::error::Error for AiError {}

// ---------------------------------------------------------------------------
// Tests -- use `mock` feature so no model file is needed.
// ---------------------------------------------------------------------------

#[cfg(test)]
#[cfg(feature = "mock")]
mod tests {
    use super::*;
    use shared::Resolution;

    #[tokio::test]
    async fn test_analyze_frame_returns_mock_data() {
        let frame = FrameData {
            camera_id: "cam-01".to_string(),
            timestamp: Utc::now(),
            jpeg_bytes: vec![0xFF, 0xD8],
            resolution: Resolution {
                width: 1920,
                height: 1080,
            },
        };

        let result = analyze_frame(&frame).await;

        assert_eq!(result.camera_id, "cam-01");
        assert_eq!(result.people_count, 3);
        assert_eq!(result.demographics.len(), 3);
        assert_eq!(result.zones.len(), 2);
        assert!(result.alerts.is_empty());
    }

    #[test]
    fn test_init_model_mock_succeeds() {
        assert!(init_model().is_ok());
    }
}
