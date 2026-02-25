# MisebanAI Quick Start -- Home Camera Setup

3 steps to turn your home camera into an AI-powered monitor.

---

## Prerequisites

- A Wi-Fi camera on your home network (RTSP-capable)
- Docker Desktop ([download](https://docker.com)) -- recommended
- macOS, Linux, or Windows with WSL2

Supported cameras: Hikvision, TAPO (TP-Link), Reolink, Dahua, SwitchBot, Amcrest, EZVIZ, Eufy, Wyze, and any ONVIF/RTSP-capable camera.

---

## Step 1: Find Your Camera

Run the camera discovery script to scan your local network:

```bash
./scripts/discover-cameras.sh
```

The script will:
- Scan your local network for devices
- Probe RTSP ports (554, 8554) and HTTP ports (80, 8080)
- Suggest RTSP URLs for major camera brands

**Example output:**
```
  [1] TP-Link TAPO
      rtsp://192.168.1.105:554/stream1

  [2] Hikvision/EZVIZ
      rtsp://192.168.1.105:554/Streaming/Channels/101

  [3] Reolink
      rtsp://192.168.1.110:554/h264Preview_01_main
```

**If your camera is not found:**

1. Make sure the camera is powered on and connected to the same Wi-Fi network
2. Enable RTSP in your camera's app:
   - **TAPO**: App > Camera Settings > Advanced Settings > Camera Account (set username/password), then use `rtsp://USER:PASS@IP:554/stream1`
   - **Reolink**: App > Device Settings > Network > Advanced > Port Settings
   - **SwitchBot**: App > Camera > Settings > Enable RTSP
3. Test the URL with VLC or ffplay:
   ```bash
   ffplay "rtsp://admin:password@192.168.1.100:554/stream1"
   ```

---

## Step 2: Install and Run

### Option A: One-command installer (recommended)

```bash
./scripts/install.sh
```

The installer will:
1. Detect Docker (or fall back to native binary)
2. Run camera discovery
3. Let you pick your camera
4. Write the config file to `~/.miseban/config.toml`
5. Start the agent

### Option B: Docker manual start

```bash
# Write config
mkdir -p ~/.miseban
cat > ~/.miseban/config.toml <<EOF
[server]
endpoint = "https://api.misebanai.com/v1/frames"
token = "YOUR_API_TOKEN"

[[cameras]]
id = "cam-1"
name = "Home Camera"
mode = "rtsp"
url = "rtsp://admin:password@192.168.1.100:554/stream1"
interval_secs = 10
EOF

# Start the agent container
docker run -d \
  --name miseban-agent \
  --restart unless-stopped \
  --network host \
  -v ~/.miseban:/root/.miseban:ro \
  miseban/agent
```

### Option C: Quick mode (no config file)

```bash
docker run -d \
  --name miseban-agent \
  --network host \
  miseban/agent \
  --camera "rtsp://admin:pass@192.168.1.100:554/stream1" \
  --token "YOUR_API_TOKEN"
```

---

## Step 3: View Your Dashboard

Open the MisebanAI dashboard in your browser:

**https://misebanai.com/dashboard/**

You will see:
- Live people count from your camera
- Time-of-day traffic patterns
- Security alerts
- AI-generated insights

---

## Useful Commands

```bash
# View agent logs
docker logs -f miseban-agent

# Stop the agent
docker stop miseban-agent

# Restart after config change
docker restart miseban-agent

# Re-scan for cameras
./scripts/discover-cameras.sh

# Test mode (capture frames to disk, no upload)
docker run --rm -v /tmp/miseban:/tmp/miseban \
  miseban/agent \
  --camera "rtsp://..." --dry-run

# Build the Docker image locally
docker build -f Dockerfile.agent -t miseban/agent .
```

---

## Common RTSP URLs by Brand

| Brand | Default RTSP URL | Notes |
|-------|-----------------|-------|
| **Hikvision** | `rtsp://admin:PASS@IP:554/Streaming/Channels/101` | Main stream. Use `/102` for sub-stream |
| **TAPO (TP-Link)** | `rtsp://USER:PASS@IP:554/stream1` | Set Camera Account in TAPO app first |
| **Reolink** | `rtsp://admin:PASS@IP:554/h264Preview_01_main` | Enable in app > Port Settings |
| **Dahua** | `rtsp://admin:PASS@IP:554/cam/realmonitor?channel=1&subtype=0` | `subtype=1` for sub-stream |
| **SwitchBot** | `rtsp://IP:8554/unicast` | No auth needed. Enable RTSP in app |
| **Amcrest** | `rtsp://admin:PASS@IP:554/cam/realmonitor?channel=1&subtype=0` | Same as Dahua protocol |
| **EZVIZ** | `rtsp://admin:PASS@IP:554/Streaming/Channels/101` | Hikvision protocol |
| **Eufy** | `rtsp://IP:554/live0` | Enable RTSP in Eufy Security app |
| **Wyze** | `rtsp://IP:8554/live` | Requires Wyze RTSP firmware |
| **Axis** | `rtsp://root:PASS@IP:554/axis-media/media.amp` | Professional cameras |
| **UniFi** | `rtsp://IP:7447/SERIAL` | Via UniFi Protect |

---

## Troubleshooting

**"Connection refused" on camera URL**
- Camera might not have RTSP enabled. Check the camera's app settings.
- Some consumer cameras (Ring, Arlo, Nest) do not support local RTSP.

**"Unauthorized" or "401" from camera**
- Wrong username/password. Most cameras use `admin` as the username.
- TAPO cameras require setting a separate "Camera Account" in the app.

**Agent can't reach the cloud API**
- Check your internet connection: `curl -I https://api.misebanai.com`
- Frames are buffered locally in `~/.miseban/buffer.db` and will upload when connection is restored.

**Docker: "network host" not working on macOS**
- macOS Docker does not support `--network host`. Use port mapping instead:
  ```bash
  docker run -d --name miseban-agent -p 3939:3939 \
    -v ~/.miseban:/root/.miseban:ro miseban/agent
  ```
  Note: the camera must be accessible from within the Docker network. If the camera is on your LAN, this may require extra Docker network configuration or using the native binary instead.
