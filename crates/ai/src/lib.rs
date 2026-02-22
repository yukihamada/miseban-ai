use chrono::Utc;
use shared::{
    AgeGroup, Alert, AnalysisResult, DemographicEstimate, FrameData, GenderEstimate,
    ZoneHeatmap,
};
use uuid::Uuid;

/// Analyze a single frame and return detection results.
///
/// # Current implementation
///
/// Returns **mock data** for development and integration testing.
///
/// # Future integration
///
/// This function will be replaced with real inference:
///
/// 1. **People detection** - YOLO v8/v9 via ONNX Runtime (or `candle`/`burn`).
///    - Input: decoded JPEG -> RGB tensor, resized to 640x640.
///    - Output: bounding boxes with confidence scores.
///
/// 2. **Demographics estimation** - Secondary classification model.
///    - Crop each detected person and run age/gender classifier.
///
/// 3. **Zone heatmap** - Map bounding-box centroids to predefined zones.
///
/// 4. **Alert generation** - Rule engine comparing detections against
///    store-specific thresholds (intrusion zones, max crowd density, etc.).
pub async fn analyze_frame(frame: &FrameData) -> AnalysisResult {
    // TODO: Replace with real ONNX / candle inference pipeline.
    //
    // Rough outline:
    //   let image = decode_jpeg(&frame.jpeg_bytes);
    //   let tensor = preprocess(image, 640, 640);
    //   let detections = yolo_session.run(tensor)?;
    //   let people = nms(detections, 0.45);
    //   let demographics = classify_demographics(&people, &image);
    //   let zones = map_to_zones(&people, &store_zone_config);
    //   let alerts = evaluate_rules(&people, &zones, &store_rules);

    let now = Utc::now();

    // Mock: pretend we detected 3 people.
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

    // Mock: no alerts in this frame.
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

#[cfg(test)]
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
}
