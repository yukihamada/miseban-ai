use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use serde::Deserialize;
use shared::{AnalysisResult, CameraConfig, FrameData, Resolution};
use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "miseban-agent", about = "MisebanAI camera agent")]
struct Cli {
    /// Path to config file. Defaults to ~/.miseban/config.toml or /etc/miseban/config.toml.
    #[arg(short, long)]
    config: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Config (TOML)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AgentConfig {
    /// Cloud API base URL (e.g. https://api.miseban.ai).
    api_url: String,
    /// API key for authentication.
    api_key: String,
    /// Store identifier.
    store_id: String,
    /// Camera definitions.
    cameras: Vec<CameraEntry>,
}

#[derive(Debug, Deserialize)]
struct CameraEntry {
    id: String,
    name: String,
    rtsp_url: String,
    /// Seconds between sampled frames (default: 5).
    #[serde(default = "default_fps_sample_rate")]
    fps_sample_rate: u64,
}

fn default_fps_sample_rate() -> u64 {
    5
}

impl From<&CameraEntry> for CameraConfig {
    fn from(e: &CameraEntry) -> Self {
        CameraConfig {
            id: e.id.clone(),
            name: e.name.clone(),
            rtsp_url: e.rtsp_url.clone(),
            fps_sample_rate: e.fps_sample_rate,
        }
    }
}

// ---------------------------------------------------------------------------
// Config resolution
// ---------------------------------------------------------------------------

fn resolve_config_path(cli_path: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = cli_path {
        return Some(p);
    }

    // ~/.miseban/config.toml
    if let Some(home) = dirs_fallback() {
        let p = home.join(".miseban").join("config.toml");
        if p.exists() {
            return Some(p);
        }
    }

    // /etc/miseban/config.toml
    let etc = PathBuf::from("/etc/miseban/config.toml");
    if etc.exists() {
        return Some(etc);
    }

    None
}

/// Simple home-dir fallback without pulling in the `dirs` crate.
fn dirs_fallback() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
}

fn load_config(path: &PathBuf) -> AgentConfig {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("Failed to read config file {}: {}", path.display(), e));
    toml::from_str(&content)
        .unwrap_or_else(|e| panic!("Failed to parse config file {}: {}", path.display(), e))
}

// ---------------------------------------------------------------------------
// Frame capture (placeholder)
// ---------------------------------------------------------------------------

/// Placeholder: pretends to capture a JPEG frame from an RTSP stream.
fn capture_frame(camera: &CameraEntry) -> FrameData {
    // TODO: Integrate with ffmpeg / GStreamer / OpenCV for real RTSP capture.
    // For now, produce a tiny 1x1 black JPEG placeholder.
    let fake_jpeg: Vec<u8> = vec![
        0xFF, 0xD8, 0xFF, 0xE0, // JPEG SOI + APP0 marker (minimal)
    ];

    FrameData {
        camera_id: camera.id.clone(),
        timestamp: chrono::Utc::now(),
        jpeg_bytes: fake_jpeg,
        resolution: Resolution {
            width: 1920,
            height: 1080,
        },
    }
}

// ---------------------------------------------------------------------------
// Upload
// ---------------------------------------------------------------------------

async fn upload_frame(
    client: &reqwest::Client,
    api_url: &str,
    api_key: &str,
    frame: &FrameData,
) -> Result<AnalysisResult, reqwest::Error> {
    let url = format!("{}/api/v1/frames", api_url);
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .json(frame)
        .send()
        .await?
        .error_for_status()?
        .json::<AnalysisResult>()
        .await?;
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Per-camera loop
// ---------------------------------------------------------------------------

async fn camera_loop(
    client: reqwest::Client,
    api_url: String,
    api_key: String,
    camera: CameraEntry,
) {
    let interval = Duration::from_secs(camera.fps_sample_rate);

    info!(
        camera_id = %camera.id,
        rtsp_url = %camera.rtsp_url,
        "Connecting to camera (placeholder)"
    );

    loop {
        let frame = capture_frame(&camera);
        info!(
            camera_id = %camera.id,
            people = "?",
            "Captured frame, uploading..."
        );

        match upload_frame(&client, &api_url, &api_key, &frame).await {
            Ok(result) => {
                info!(
                    camera_id = %camera.id,
                    people_count = result.people_count,
                    alerts = result.alerts.len(),
                    "Analysis result received"
                );
            }
            Err(e) => {
                warn!(
                    camera_id = %camera.id,
                    error = %e,
                    "Upload failed, will retry next cycle"
                );
            }
        }

        tokio::time::sleep(interval).await;
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    let config_path = resolve_config_path(cli.config).unwrap_or_else(|| {
        error!("No config file found. Create ~/.miseban/config.toml or pass --config");
        std::process::exit(1);
    });

    info!(path = %config_path.display(), "Loading config");
    let config = load_config(&config_path);

    info!(
        store_id = %config.store_id,
        cameras = config.cameras.len(),
        api_url = %config.api_url,
        "MisebanAI agent starting"
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("Failed to build HTTP client");

    // Spawn one task per camera.
    let mut handles = Vec::new();
    for cam in config.cameras {
        let client = client.clone();
        let api_url = config.api_url.clone();
        let api_key = config.api_key.clone();
        handles.push(tokio::spawn(camera_loop(client, api_url, api_key, cam)));
    }

    // Wait forever (all camera loops are infinite).
    for h in handles {
        if let Err(e) = h.await {
            error!(error = %e, "Camera task panicked");
        }
    }
}
