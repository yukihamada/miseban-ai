use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use serde::Deserialize;
use shared::{AnalysisResult, CameraConfig, FrameData, Resolution};
use tokio::signal;
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

mod buffer;
mod scanner;
mod setup;

use buffer::FrameBuffer;

// ---------------------------------------------------------------------------
// Version
// ---------------------------------------------------------------------------

const VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "miseban-agent",
    about = "MisebanAI camera agent — plug in, power on, done.",
    version
)]
struct Cli {
    /// Path to config file (TOML). Defaults to ~/.miseban/config.toml or /etc/miseban/config.toml.
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Quick mode: single camera RTSP URL (no config file needed).
    #[arg(long, value_name = "URL")]
    camera: Option<String>,

    /// API token for quick mode (required with --camera).
    #[arg(long, value_name = "TOKEN")]
    token: Option<String>,

    /// API endpoint for quick mode.
    #[arg(long, default_value = "https://api.misebanai.com/api/v1/frames")]
    endpoint: String,

    /// Capture frames but don't upload. Saves JPEGs to /tmp/miseban/ for testing.
    #[arg(long)]
    dry_run: bool,

    /// Force setup wizard (Web UI on port 3939).
    #[arg(long)]
    setup: bool,

    /// Enable verbose logging (debug level).
    #[arg(short, long)]
    verbose: bool,
}

// ---------------------------------------------------------------------------
// Config (TOML)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Clone)]
struct AgentConfig {
    server: ServerConfig,
    cameras: Vec<CameraEntry>,
}

#[derive(Debug, Deserialize, Clone)]
struct ServerConfig {
    /// Cloud API endpoint URL (e.g. https://api.misebanai.com/api/v1/frames).
    endpoint: String,
    /// API token for authentication.
    token: String,
}

#[derive(Debug, Deserialize, Clone)]
struct CameraEntry {
    /// Unique camera identifier.
    id: String,
    /// Human-readable name.
    name: String,
    /// Capture mode: "snapshot" (HTTP GET) or "rtsp" (ffmpeg subprocess).
    #[serde(default = "default_mode")]
    mode: String,
    /// Camera URL. For snapshot mode: HTTP URL (e.g. http://192.168.1.100/snap.jpg).
    /// For rtsp mode: RTSP URL (e.g. rtsp://admin:pass@192.168.1.100/stream1).
    url: String,
    /// Seconds between frame captures.
    #[serde(default = "default_interval")]
    interval_secs: u64,
    /// HTTP basic auth username (optional, for snapshot mode).
    username: Option<String>,
    /// HTTP basic auth password (optional, for snapshot mode).
    password: Option<String>,
}

fn default_mode() -> String {
    "snapshot".to_string()
}

fn default_interval() -> u64 {
    5
}

impl From<&CameraEntry> for CameraConfig {
    fn from(e: &CameraEntry) -> Self {
        CameraConfig {
            id: e.id.clone(),
            name: e.name.clone(),
            rtsp_url: e.url.clone(),
            fps_sample_rate: e.interval_secs,
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
    std::env::var("HOME").ok().map(PathBuf::from)
}

fn load_config(path: &PathBuf) -> AgentConfig {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("Failed to read config file {}: {}", path.display(), e));
    toml::from_str(&content)
        .unwrap_or_else(|e| panic!("Failed to parse config file {}: {}", path.display(), e))
}

// ---------------------------------------------------------------------------
// Frame capture: HTTP snapshot mode
// ---------------------------------------------------------------------------

async fn capture_snapshot(
    client: &reqwest::Client,
    camera: &CameraEntry,
) -> Result<Vec<u8>, String> {
    let mut request = client.get(&camera.url);

    // Apply basic auth if credentials are provided.
    if let (Some(user), Some(pass)) = (&camera.username, &camera.password) {
        request = request.basic_auth(user, Some(pass));
    }

    let response = request
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("HTTP {} from {}", response.status(), camera.url));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("Failed to read response body: {}", e))?;

    if bytes.len() < 2 {
        return Err("Response too small to be a valid JPEG".to_string());
    }

    // Validate JPEG magic bytes.
    if bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return Err(format!(
            "Response does not look like JPEG (first bytes: {:02X} {:02X})",
            bytes[0], bytes[1]
        ));
    }

    info!(
        camera_id = %camera.id,
        bytes = bytes.len(),
        "Snapshot captured via HTTP"
    );

    Ok(bytes.to_vec())
}

// ---------------------------------------------------------------------------
// Frame capture: FFmpeg RTSP mode
// ---------------------------------------------------------------------------

async fn capture_rtsp_ffmpeg(camera: &CameraEntry) -> Result<Vec<u8>, String> {
    use tokio::process::Command;

    debug!(
        camera_id = %camera.id,
        url = %camera.url,
        "Spawning ffmpeg for RTSP capture"
    );

    let output = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-rtsp_transport",
            "tcp",
            "-i",
            &camera.url,
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-vcodec",
            "mjpeg",
            "pipe:1",
        ])
        .output()
        .await
        .map_err(|e| format!("Failed to spawn ffmpeg (is it installed?): {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "ffmpeg exited with {}: {}",
            output.status,
            stderr.trim()
        ));
    }

    let bytes = output.stdout;

    if bytes.len() < 2 {
        return Err("ffmpeg produced no output".to_string());
    }

    if bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return Err(format!(
            "ffmpeg output does not look like JPEG (first bytes: {:02X} {:02X}, {} bytes total)",
            bytes[0],
            bytes[1],
            bytes.len()
        ));
    }

    info!(
        camera_id = %camera.id,
        bytes = bytes.len(),
        "Frame captured via ffmpeg/RTSP"
    );

    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Frame capture dispatcher
// ---------------------------------------------------------------------------

async fn capture_frame(
    client: &reqwest::Client,
    camera: &CameraEntry,
) -> Result<FrameData, String> {
    let jpeg_bytes = match camera.mode.as_str() {
        "snapshot" => capture_snapshot(client, camera).await?,
        "rtsp" => capture_rtsp_ffmpeg(camera).await?,
        other => {
            return Err(format!(
                "Unknown capture mode '{}'. Use 'snapshot' or 'rtsp'.",
                other
            ));
        }
    };

    // We don't parse JPEG headers for resolution here; use a sensible default.
    // The server can extract actual resolution from the JPEG if needed.
    Ok(FrameData {
        camera_id: camera.id.clone(),
        timestamp: chrono::Utc::now(),
        jpeg_bytes,
        resolution: Resolution {
            width: 0,
            height: 0,
        },
    })
}

// ---------------------------------------------------------------------------
// Upload
// ---------------------------------------------------------------------------

async fn upload_frame(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    frame: &FrameData,
) -> Result<Option<AnalysisResult>, String> {
    let now = frame.timestamp.to_rfc3339();

    let resp = client
        .post(endpoint)
        .bearer_auth(token)
        .header("Content-Type", "image/jpeg")
        .header("X-Camera-Id", &frame.camera_id)
        .header("X-Timestamp", &now)
        .body(frame.jpeg_bytes.clone())
        .send()
        .await
        .map_err(|e| format!("Upload request failed: {}", e))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable>".to_string());
        return Err(format!("Upload returned HTTP {}: {}", status, body));
    }

    // Try to parse analysis result; server might return empty or non-JSON.
    let body = resp
        .bytes()
        .await
        .map_err(|e| format!("Failed to read upload response: {}", e))?;
    if body.is_empty() {
        return Ok(None);
    }

    match serde_json::from_slice::<AnalysisResult>(&body) {
        Ok(result) => Ok(Some(result)),
        Err(_) => {
            debug!("Upload succeeded but response is not AnalysisResult JSON");
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// Dry-run: save to disk
// ---------------------------------------------------------------------------

async fn save_frame_to_disk(frame: &FrameData) -> Result<(), String> {
    let dir = PathBuf::from("/tmp/miseban");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("Failed to create /tmp/miseban: {}", e))?;

    let filename = format!(
        "{}_{}.jpg",
        frame.camera_id,
        frame.timestamp.format("%Y%m%d_%H%M%S")
    );
    let path = dir.join(&filename);

    tokio::fs::write(&path, &frame.jpeg_bytes)
        .await
        .map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;

    info!(
        camera_id = %frame.camera_id,
        path = %path.display(),
        bytes = frame.jpeg_bytes.len(),
        "[dry-run] Frame saved to disk"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Per-camera loop
// ---------------------------------------------------------------------------

async fn camera_loop(
    client: reqwest::Client,
    endpoint: String,
    token: String,
    camera: CameraEntry,
    dry_run: bool,
    frame_buffer: Arc<FrameBuffer>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let interval = Duration::from_secs(camera.interval_secs);

    info!(
        camera_id = %camera.id,
        name = %camera.name,
        mode = %camera.mode,
        url = %camera.url,
        interval_secs = camera.interval_secs,
        "Camera loop started"
    );

    let mut consecutive_errors: u32 = 0;

    loop {
        // Capture frame.
        match capture_frame(&client, &camera).await {
            Ok(frame) => {
                consecutive_errors = 0;

                if dry_run {
                    if let Err(e) = save_frame_to_disk(&frame).await {
                        warn!(camera_id = %camera.id, error = %e, "Failed to save frame to disk");
                    }
                } else {
                    // Enqueue to local SQLite buffer first (never lose frames).
                    let buf_id = match frame_buffer.enqueue(&frame).await {
                        Ok(id) => id,
                        Err(e) => {
                            warn!(camera_id = %camera.id, error = %e, "Failed to enqueue frame to buffer");
                            // Fall through — attempt direct upload anyway.
                            -1
                        }
                    };

                    // Attempt immediate upload.
                    match upload_frame(&client, &endpoint, &token, &frame).await {
                        Ok(Some(result)) => {
                            info!(
                                camera_id = %camera.id,
                                people_count = result.people_count,
                                alerts = result.alerts.len(),
                                "Analysis result received"
                            );
                            if buf_id > 0 {
                                let _ = frame_buffer.mark_done(buf_id).await;
                            }
                        }
                        Ok(None) => {
                            debug!(camera_id = %camera.id, "Frame uploaded (no analysis result)");
                            if buf_id > 0 {
                                let _ = frame_buffer.mark_done(buf_id).await;
                            }
                        }
                        Err(e) => {
                            warn!(
                                camera_id = %camera.id,
                                error = %e,
                                "Upload failed — frame buffered locally for retry"
                            );
                            if buf_id > 0 {
                                let _ = frame_buffer.mark_failed(buf_id).await;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                warn!(
                    camera_id = %camera.id,
                    error = %e,
                    consecutive_errors,
                    "Frame capture failed"
                );

                if consecutive_errors >= 10 {
                    error!(
                        camera_id = %camera.id,
                        "10 consecutive capture failures — backing off for 30s"
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                        _ = shutdown_rx.changed() => {
                            info!(camera_id = %camera.id, "Shutdown signal received during backoff");
                            return;
                        }
                    }
                    consecutive_errors = 0;
                    continue;
                }
            }
        }

        // Sleep until next capture, but respect shutdown.
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown_rx.changed() => {
                info!(camera_id = %camera.id, "Shutdown signal received");
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Background flush: retry pending frames from the buffer
// ---------------------------------------------------------------------------

async fn flush_pending(
    client: reqwest::Client,
    endpoint: String,
    token: String,
    frame_buffer: Arc<FrameBuffer>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    const FLUSH_INTERVAL: Duration = Duration::from_secs(30);
    const BATCH_SIZE: usize = 10;
    const CLEANUP_HOURS: u64 = 24;

    info!(
        "Background flush task started (every {}s, batch {})",
        FLUSH_INTERVAL.as_secs(),
        BATCH_SIZE
    );

    loop {
        tokio::select! {
            _ = tokio::time::sleep(FLUSH_INTERVAL) => {}
            _ = shutdown_rx.changed() => {
                info!("Flush task received shutdown signal");
                return;
            }
        }

        // Fetch pending frames.
        let pending = match frame_buffer.peek_pending(BATCH_SIZE).await {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "Failed to peek pending frames");
                continue;
            }
        };

        if pending.is_empty() {
            // Periodic cleanup even when queue is empty.
            let _ = frame_buffer.cleanup_old(CLEANUP_HOURS).await;
            continue;
        }

        info!(count = pending.len(), "Flushing buffered frames");

        for bf in &pending {
            let frame = FrameData {
                camera_id: bf.camera_id.clone(),
                timestamp: chrono::DateTime::parse_from_rfc3339(&bf.timestamp)
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now()),
                jpeg_bytes: bf.jpeg_bytes.clone(),
                resolution: Resolution {
                    width: 0,
                    height: 0,
                },
            };

            match upload_frame(&client, &endpoint, &token, &frame).await {
                Ok(_) => {
                    debug!(id = bf.id, camera_id = %bf.camera_id, "Buffered frame uploaded");
                    let _ = frame_buffer.mark_done(bf.id).await;
                }
                Err(e) => {
                    warn!(
                        id = bf.id,
                        camera_id = %bf.camera_id,
                        retry_count = bf.retry_count,
                        error = %e,
                        "Buffered frame upload failed"
                    );
                    let _ = frame_buffer.mark_failed(bf.id).await;
                    // Stop batch on first failure (network likely still down).
                    break;
                }
            }
        }

        // Cleanup completed + expired entries.
        let _ = frame_buffer.cleanup_old(CLEANUP_HOURS).await;
    }
}

// ---------------------------------------------------------------------------
// Startup banner
// ---------------------------------------------------------------------------

fn print_banner(config: &AgentConfig, dry_run: bool, buffer_path: &Path, pending: usize) {
    println!();
    println!("  ╔══════════════════════════════════════════╗");
    println!("  ║         MisebanAI Camera Agent           ║");
    println!("  ║              v{}                      ║", VERSION);
    println!("  ╚══════════════════════════════════════════╝");
    println!();
    println!("  Endpoint : {}", config.server.endpoint);
    println!(
        "  Token    : {}...",
        &config.server.token.get(..8).unwrap_or("****")
    );
    println!("  Dry-run  : {}", dry_run);
    println!("  Cameras  : {}", config.cameras.len());
    println!("  Buffer   : {}", buffer_path.display());
    if pending > 0 {
        println!(
            "  Pending  : {} frames queued from previous session",
            pending
        );
    }
    println!();
    for cam in &config.cameras {
        println!("    [{:^12}] {} ({})", cam.id, cam.name, cam.mode);
        println!("    {:>14} URL: {}", "", cam.url);
        println!("    {:>14} Interval: {}s", "", cam.interval_secs);
    }
    println!();
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Initialise tracing.
    let env_filter = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| env_filter.into()),
        )
        .init();

    // -----------------------------------------------------------------------
    // Auto-detect: no config? → launch setup wizard automatically.
    // -----------------------------------------------------------------------
    if cli.setup {
        println!();
        println!("  ╔══════════════════════════════════════════╗");
        println!("  ║     MisebanAI セットアップウィザード       ║");
        println!("  ╚══════════════════════════════════════════╝");
        println!();
        println!("  ブラウザで http://localhost:3939 を開いてください");
        println!("  スマホからも接続できます（同じWi-Fi）");
        println!();
        if let Err(e) = setup::run_setup_server().await {
            error!(error = %e, "Setup server failed");
            std::process::exit(1);
        }
        info!("Setup complete! Restarting in capture mode...");
        // Fall through to load the freshly-written config.
    }

    let has_config = resolve_config_path(cli.config.clone()).is_some();
    let has_quick_mode = cli.camera.is_some();

    if !has_config && !has_quick_mode {
        // No config found — auto-enter setup mode (おばあちゃんモード).
        println!();
        println!("  ╔══════════════════════════════════════════╗");
        println!("  ║         MisebanAI Camera Agent           ║");
        println!("  ║              v{}                      ║", VERSION);
        println!("  ╚══════════════════════════════════════════╝");
        println!();
        println!("  設定ファイルが見つかりません。");
        println!("  セットアップウィザードを起動します...");
        println!();
        println!("  ブラウザで http://localhost:3939 を開いてください");
        println!("  スマホからも接続できます（同じWi-Fi）");
        println!();
        if let Err(e) = setup::run_setup_server().await {
            error!(error = %e, "Setup server failed");
            std::process::exit(1);
        }
        info!("Setup complete! Starting capture mode...");
    }

    // Build config: either from --camera quick mode or from config file.
    let config = if let Some(camera_url) = &cli.camera {
        // Quick mode: single camera from CLI args.
        let token = cli.token.clone().unwrap_or_else(|| {
            error!("--token is required when using --camera");
            std::process::exit(1);
        });

        // Auto-detect mode from URL scheme.
        let mode = if camera_url.starts_with("rtsp://") || camera_url.starts_with("rtsps://") {
            "rtsp"
        } else {
            "snapshot"
        };

        AgentConfig {
            server: ServerConfig {
                endpoint: cli.endpoint.clone(),
                token,
            },
            cameras: vec![CameraEntry {
                id: "cam-cli".to_string(),
                name: "CLI Camera".to_string(),
                mode: mode.to_string(),
                url: camera_url.clone(),
                interval_secs: 5,
                username: None,
                password: None,
            }],
        }
    } else {
        // Config file mode.
        let config_path = resolve_config_path(cli.config).unwrap_or_else(|| {
            error!("Config file still not found after setup. Exiting.");
            std::process::exit(1);
        });

        info!(path = %config_path.display(), "Loading config");
        load_config(&config_path)
    };

    if config.cameras.is_empty() {
        error!("No cameras configured");
        std::process::exit(1);
    }

    // Initialise SQLite frame buffer (zero-config: ~/.miseban/buffer.db).
    let buffer_path = dirs_fallback()
        .map(|h| h.join(".miseban").join("buffer.db"))
        .unwrap_or_else(|| PathBuf::from("/tmp/miseban/buffer.db"));

    let frame_buffer = Arc::new(FrameBuffer::open(&buffer_path).await.unwrap_or_else(|e| {
        error!(error = %e, "Failed to open frame buffer DB");
        std::process::exit(1);
    }));

    let pending = frame_buffer.pending_count().await.unwrap_or(0);

    // Print startup banner.
    print_banner(&config, cli.dry_run, &buffer_path, pending);

    // Build HTTP client.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("Failed to build HTTP client");

    // Shutdown channel.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Clone server config before consuming cameras.
    let endpoint = config.server.endpoint.clone();
    let token = config.server.token.clone();

    // Spawn one task per camera.
    let mut handles = Vec::new();
    for cam in config.cameras {
        let client = client.clone();
        let endpoint = endpoint.clone();
        let token = token.clone();
        let dry_run = cli.dry_run;
        let buf = Arc::clone(&frame_buffer);
        let rx = shutdown_rx.clone();
        handles.push(tokio::spawn(camera_loop(
            client, endpoint, token, cam, dry_run, buf, rx,
        )));
    }

    // Spawn background flush task (retries pending frames every 30s).
    if !cli.dry_run {
        let flush_handle = tokio::spawn(flush_pending(
            client.clone(),
            endpoint,
            token,
            Arc::clone(&frame_buffer),
            shutdown_rx.clone(),
        ));
        handles.push(flush_handle);
    }

    // Wait for SIGINT or SIGTERM for graceful shutdown.
    tokio::select! {
        _ = signal::ctrl_c() => {
            info!("Received SIGINT (Ctrl+C), shutting down gracefully...");
        }
        _ = async {
            #[cfg(unix)]
            {
                let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
                    .expect("Failed to register SIGTERM handler");
                sigterm.recv().await;
            }
            #[cfg(not(unix))]
            {
                // On non-Unix, just wait forever (ctrl_c will fire).
                std::future::pending::<()>().await;
            }
        } => {
            info!("Received SIGTERM, shutting down gracefully...");
        }
    }

    // Signal all camera loops to stop.
    let _ = shutdown_tx.send(true);

    // Give camera loops a moment to finish their current cycle.
    let shutdown_timeout = Duration::from_secs(5);
    info!(
        "Waiting up to {:?} for camera loops to finish...",
        shutdown_timeout
    );

    let _ = tokio::time::timeout(shutdown_timeout, async {
        for h in handles {
            let _ = h.await;
        }
    })
    .await;

    info!("MisebanAI agent stopped.");
}
