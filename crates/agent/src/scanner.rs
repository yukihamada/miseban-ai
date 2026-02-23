//! Network camera auto-discovery.
//!
//! Scans the local subnet for cameras by probing common RTSP/HTTP snapshot
//! ports. No ONVIF dependency — just raw TCP connect + HTTP probing.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use serde::Serialize;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info};

/// A camera discovered on the local network.
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredCamera {
    pub ip: String,
    pub port: u16,
    pub protocol: String, // "rtsp" | "http"
    pub url: String,
    pub name: String,
}

/// Common ports to probe for IP cameras.
const RTSP_PORTS: &[u16] = &[554, 8554, 8080];
const HTTP_PORTS: &[u16] = &[80, 8080, 8000, 8888];

/// Common RTSP path patterns for popular camera brands.
#[allow(dead_code)] // Intended for future use: RTSP stream validation
const RTSP_PATHS: &[&str] = &[
    "/stream1",
    "/h264Preview_01_main",
    "/cam/realmonitor?channel=1&subtype=0",
    "/live/ch00_0",
    "/Streaming/Channels/101",
    "/",
];

/// Common HTTP snapshot paths.
const SNAPSHOT_PATHS: &[&str] = &[
    "/snap.jpg",
    "/snapshot.jpg",
    "/cgi-bin/snapshot.cgi",
    "/image/jpeg.cgi",
    "/jpg/image.jpg",
    "/capture",
    "/tmpfs/auto.jpg",
];

/// Get the local IPv4 subnet to scan (assumes /24).
fn get_local_subnet() -> Option<Ipv4Addr> {
    // Use a UDP socket trick to find the default interface IP.
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let local_addr = socket.local_addr().ok()?;
    match local_addr.ip() {
        IpAddr::V4(ip) => Some(ip),
        _ => None,
    }
}

/// Check if a TCP port is open on the given IP (with timeout).
async fn is_port_open(ip: Ipv4Addr, port: u16) -> bool {
    let addr = SocketAddr::new(IpAddr::V4(ip), port);
    timeout(Duration::from_millis(300), TcpStream::connect(addr))
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false)
}

/// Try HTTP snapshot paths to see if any return JPEG data.
async fn probe_http_snapshot(ip: Ipv4Addr, port: u16) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?;

    for path in SNAPSHOT_PATHS {
        let url = format!("http://{}:{}{}", ip, port, path);
        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().is_success() {
                if let Some(ct) = resp.headers().get("content-type") {
                    let ct_str = ct.to_str().unwrap_or("");
                    if ct_str.contains("image/jpeg") || ct_str.contains("image/jpg") {
                        return Some(url);
                    }
                }
                // Some cameras don't set content-type; check JPEG magic bytes.
                if let Ok(bytes) = resp.bytes().await {
                    if bytes.len() > 2 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
                        return Some(url);
                    }
                }
            }
        }
    }
    None
}

/// Scan the local /24 subnet for cameras. Returns discovered cameras.
pub async fn scan_network() -> Vec<DiscoveredCamera> {
    let mut cameras = Vec::new();

    let local_ip = match get_local_subnet() {
        Some(ip) => ip,
        None => {
            info!("Could not determine local IP address");
            return cameras;
        }
    };

    let octets = local_ip.octets();
    info!(
        local_ip = %local_ip,
        subnet = format!("{}.{}.{}.0/24", octets[0], octets[1], octets[2]),
        "Scanning local network for cameras..."
    );

    // Scan all IPs in the /24 subnet in parallel (batches of 32).
    let mut all_tasks = Vec::new();

    for host in 1..=254u8 {
        let ip = Ipv4Addr::new(octets[0], octets[1], octets[2], host);
        if ip == local_ip {
            continue; // Skip self.
        }
        all_tasks.push(tokio::spawn(scan_single_host(ip)));
    }

    for task in all_tasks {
        if let Ok(mut found) = task.await {
            cameras.append(&mut found);
        }
    }

    info!(count = cameras.len(), "Camera scan complete");
    cameras
}

/// Scan a single host for camera services.
async fn scan_single_host(ip: Ipv4Addr) -> Vec<DiscoveredCamera> {
    let mut found = Vec::new();

    // Check RTSP ports.
    for &port in RTSP_PORTS {
        if is_port_open(ip, port).await {
            debug!(ip = %ip, port, "RTSP port open");
            // We can't easily validate RTSP without a full client.
            // Add the first common path as a suggestion.
            found.push(DiscoveredCamera {
                ip: ip.to_string(),
                port,
                protocol: "rtsp".to_string(),
                url: format!("rtsp://{}:{}/stream1", ip, port),
                name: format!("Camera {}", ip),
            });
        }
    }

    // Check HTTP ports for snapshot endpoints.
    for &port in HTTP_PORTS {
        if is_port_open(ip, port).await {
            if let Some(snapshot_url) = probe_http_snapshot(ip, port).await {
                debug!(ip = %ip, port, url = %snapshot_url, "HTTP snapshot found");
                found.push(DiscoveredCamera {
                    ip: ip.to_string(),
                    port,
                    protocol: "http".to_string(),
                    url: snapshot_url,
                    name: format!("Camera {}", ip),
                });
            }
        }
    }

    found
}

/// Quick scan: just check if any camera-like ports are open (fast pre-check).
#[allow(dead_code)] // Intended for future use: setup wizard pre-check
pub async fn quick_check() -> bool {
    let local_ip = match get_local_subnet() {
        Some(ip) => ip,
        None => return false,
    };

    let octets = local_ip.octets();

    // Check a few common camera IPs in the subnet (gateway+1, +100..+110).
    let candidates: Vec<u8> = (100..=110).chain(2..=5).collect();

    for host in candidates {
        let ip = Ipv4Addr::new(octets[0], octets[1], octets[2], host);
        for &port in &[554u16, 80, 8080] {
            if is_port_open(ip, port).await {
                return true;
            }
        }
    }
    false
}
