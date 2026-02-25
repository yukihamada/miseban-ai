#!/usr/bin/env bash
# ============================================================================
# MisebanAI Camera Agent — Home Installer
# Usage: curl -fsSL https://misebanai.com/install.sh | bash
#
# Supports two modes:
#   1. Docker mode (recommended): runs the agent in a container
#   2. Binary mode (fallback): downloads or builds the native binary
# ============================================================================
set -euo pipefail

REPO="yukihamada/miseban-ai"
BINARY="miseban-agent"
INSTALL_DIR="/usr/local/bin"
CONFIG_DIR="$HOME/.miseban"
SERVICE_NAME="miseban-agent"
DOCKER_IMAGE="miseban/agent"
SETUP_PORT="3939"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

echo ""
echo "  ╔══════════════════════════════════════════╗"
echo "  ║     MisebanAI Camera Agent Installer     ║"
echo "  ╚══════════════════════════════════════════╝"
echo ""

# ---------------------------------------------------------------------------
# 1. Detect OS & architecture
# ---------------------------------------------------------------------------
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$ARCH" in
    x86_64|amd64)  ARCH="x86_64" ;;
    aarch64|arm64) ARCH="aarch64" ;;
    armv7l|armhf)  ARCH="armv7" ;;
    *)
        echo -e "${RED}Unsupported architecture: $ARCH${NC}"
        exit 1
        ;;
esac

echo -e "${CYAN}Platform: ${OS}/${ARCH}${NC}"
echo ""

# ---------------------------------------------------------------------------
# 2. Choose install mode: Docker (preferred) or Binary
# ---------------------------------------------------------------------------
USE_DOCKER=false

if command -v docker &>/dev/null; then
    if docker info &>/dev/null 2>&1; then
        echo -e "${GREEN}Docker detected and running.${NC}"
        USE_DOCKER=true
    else
        echo -e "${YELLOW}Docker found but not running. Trying binary install instead.${NC}"
    fi
else
    echo -e "${YELLOW}Docker not found. Installing as native binary.${NC}"
    echo -e "${YELLOW}(For the easiest experience, install Docker: https://docker.com)${NC}"
fi
echo ""

# ---------------------------------------------------------------------------
# 3. Camera Discovery
# ---------------------------------------------------------------------------
echo -e "${BOLD}Step 1: Camera Discovery${NC}"
echo "  Scanning your network for cameras..."
echo ""

DISCOVERED_CAMERAS=""

# Try to find the discover script, or use inline logic
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" 2>/dev/null && pwd)" || SCRIPT_DIR=""
DISCOVER_SCRIPT="${SCRIPT_DIR}/discover-cameras.sh"

if [ -f "$DISCOVER_SCRIPT" ]; then
    # Use the discovery script in JSON mode
    DISCOVERED_CAMERAS=$("$DISCOVER_SCRIPT" --json 2>/dev/null) || true
else
    # Inline minimal discovery: scan for RTSP on common IPs
    echo -e "  ${CYAN}Quick scan: checking common camera ports...${NC}"

    # Get local subnet
    LOCAL_IP=""
    if [ "$OS" = "darwin" ]; then
        for iface in en0 en1 en2; do
            LOCAL_IP=$(ipconfig getifaddr "$iface" 2>/dev/null) && [ -n "$LOCAL_IP" ] && break
        done
    fi
    if [ -z "$LOCAL_IP" ] && command -v ip &>/dev/null; then
        LOCAL_IP=$(ip route get 8.8.8.8 2>/dev/null | sed -n 's/.*src \([0-9.]*\).*/\1/p' | head -1)
    fi

    if [ -n "$LOCAL_IP" ]; then
        SUBNET=$(echo "$LOCAL_IP" | sed 's/\.[0-9]*$//')
        echo -e "  ${CYAN}Your IP: $LOCAL_IP (subnet: ${SUBNET}.0/24)${NC}"

        # Check ARP table for known hosts
        HOSTS=$(arp -a 2>/dev/null | sed -n 's/.*(\([0-9]*\.[0-9]*\.[0-9]*\.[0-9]*\)).*/\1/p' | sort -u || true)

        for ip in $HOSTS; do
            ip_subnet=$(echo "$ip" | sed 's/\.[0-9]*$//')
            [ "$ip_subnet" != "$SUBNET" ] && continue
            [ "$ip" = "$LOCAL_IP" ] && continue

            for port in 554 8554; do
                if nc -z -w 1 "$ip" "$port" 2>/dev/null; then
                    echo -e "  ${GREEN}Found camera: ${BOLD}${ip}:${port}${NC} (RTSP)"
                    DISCOVERED_CAMERAS="${DISCOVERED_CAMERAS}rtsp://${ip}:${port}/stream1
"
                fi
            done
        done
    fi
fi
echo ""

# ---------------------------------------------------------------------------
# 4. Let user select camera URL
# ---------------------------------------------------------------------------
echo -e "${BOLD}Step 2: Camera Configuration${NC}"
echo ""

# Parse discovered cameras into a selection list
CAMERA_URLS=""
CAM_COUNT=0

if [ -n "$DISCOVERED_CAMERAS" ] && [ "$DISCOVERED_CAMERAS" != "[]" ]; then
    # If JSON mode was used, extract URLs
    if echo "$DISCOVERED_CAMERAS" | grep -q '^\['; then
        # JSON array — extract rtsp URLs (unique by IP:port)
        CAMERA_URLS=$(echo "$DISCOVERED_CAMERAS" | \
            sed 's/},/}\n/g' | \
            sed -n 's/.*"url":"\([^"]*\)".*/\1/p' | \
            awk -F/ '{print $1"//"$3}' | sort -u | head -20)
        # For each unique IP:port, pick the best path guess
        TEMP_URLS=""
        for base_url in $CAMERA_URLS; do
            # Just suggest the most common paths
            ip_port=$(echo "$base_url" | sed 's|rtsp://||')
            TEMP_URLS="${TEMP_URLS}rtsp://${ip_port}/stream1
rtsp://${ip_port}/Streaming/Channels/101
rtsp://${ip_port}/h264Preview_01_main
"
        done
        CAMERA_URLS="$TEMP_URLS"
    else
        # Plain text (one URL per line from inline scan)
        CAMERA_URLS="$DISCOVERED_CAMERAS"
    fi

    echo "  Cameras found on your network:"
    echo ""
    IDX=1
    echo "$CAMERA_URLS" | while IFS= read -r url; do
        [ -z "$url" ] && continue
        echo -e "    ${GREEN}[$IDX]${NC} $url"
        IDX=$((IDX + 1))
    done
    CAM_COUNT=$(echo "$CAMERA_URLS" | grep -c 'rtsp://' || true)
    echo ""
    echo -e "    ${YELLOW}[M]${NC} Enter camera URL manually"
    echo ""
else
    echo "  No cameras found automatically."
    echo ""
    echo -e "  ${YELLOW}Common RTSP URLs by brand:${NC}"
    echo "    Hikvision : rtsp://admin:PASS@IP:554/Streaming/Channels/101"
    echo "    TAPO      : rtsp://USER:PASS@IP:554/stream1"
    echo "    Reolink   : rtsp://admin:PASS@IP:554/h264Preview_01_main"
    echo "    Dahua     : rtsp://admin:PASS@IP:554/cam/realmonitor?channel=1&subtype=0"
    echo "    SwitchBot : rtsp://IP:8554/unicast"
    echo ""
fi

CAMERA_URL=""
while [ -z "$CAMERA_URL" ]; do
    echo -ne "  ${BOLD}Enter camera RTSP URL${NC} (or number from list): "
    read -r CHOICE

    # If user entered a number, look it up
    if echo "$CHOICE" | grep -qE '^[0-9]+$' && [ -n "$CAMERA_URLS" ]; then
        CAMERA_URL=$(echo "$CAMERA_URLS" | sed -n "${CHOICE}p")
        if [ -z "$CAMERA_URL" ]; then
            echo -e "  ${RED}Invalid number. Try again.${NC}"
            continue
        fi
    elif echo "$CHOICE" | grep -qE '^rtsp://'; then
        CAMERA_URL="$CHOICE"
    elif echo "$CHOICE" | grep -qE '^http'; then
        CAMERA_URL="$CHOICE"
    elif [ -n "$CHOICE" ]; then
        # Assume it's an IP with default RTSP
        CAMERA_URL="rtsp://${CHOICE}:554/stream1"
    else
        echo -e "  ${RED}Please enter a URL or select a number.${NC}"
        continue
    fi
done
echo ""
echo -e "  ${GREEN}Camera URL: ${BOLD}${CAMERA_URL}${NC}"
echo ""

# ---------------------------------------------------------------------------
# 5. Get API token
# ---------------------------------------------------------------------------
echo -e "${BOLD}Step 3: API Token${NC}"
echo ""
echo "  Get your token from: https://misebanai.com/dashboard/"
echo ""
echo -ne "  ${BOLD}API Token${NC}: "
read -r API_TOKEN

if [ -z "$API_TOKEN" ]; then
    echo -e "${YELLOW}No token entered. You can add it later in ${CONFIG_DIR}/config.toml${NC}"
    API_TOKEN="your-api-token-here"
fi
echo ""

# ---------------------------------------------------------------------------
# 6. Write config.toml
# ---------------------------------------------------------------------------
mkdir -p "${CONFIG_DIR}"

# Auto-detect mode
if echo "$CAMERA_URL" | grep -qE '^rtsp://'; then
    CAM_MODE="rtsp"
else
    CAM_MODE="snapshot"
fi

cat > "${CONFIG_DIR}/config.toml" <<TOML
# MisebanAI Agent Config (auto-generated by installer)

[server]
endpoint = "https://api.misebanai.com/v1/frames"
token = "${API_TOKEN}"

[[cameras]]
id = "cam-1"
name = "ホームカメラ"
mode = "${CAM_MODE}"
url = "${CAMERA_URL}"
interval_secs = 10
TOML

echo -e "${GREEN}Config saved to ${CONFIG_DIR}/config.toml${NC}"
echo ""

# ---------------------------------------------------------------------------
# 7A. Docker install
# ---------------------------------------------------------------------------
if [ "$USE_DOCKER" = "true" ]; then
    echo -e "${BOLD}Step 4: Starting with Docker${NC}"
    echo ""

    # Stop existing container if running
    if docker ps -q -f name=miseban-agent 2>/dev/null | grep -q .; then
        echo -e "${YELLOW}Stopping existing miseban-agent container...${NC}"
        docker stop miseban-agent >/dev/null 2>&1 || true
        docker rm miseban-agent >/dev/null 2>&1 || true
    fi

    # Pull or build the image
    echo -e "${CYAN}Pulling ${DOCKER_IMAGE}...${NC}"
    if ! docker pull "${DOCKER_IMAGE}" 2>/dev/null; then
        echo -e "${YELLOW}Pre-built image not available. Building locally...${NC}"

        # Check if we have the source code
        if [ -f "$(dirname "$SCRIPT_DIR")/Dockerfile.agent" ]; then
            cd "$(dirname "$SCRIPT_DIR")"
            docker build -f Dockerfile.agent -t "${DOCKER_IMAGE}" .
        else
            echo -e "${YELLOW}Source not found. Cloning repository...${NC}"
            TMP_DIR=$(mktemp -d)
            git clone --depth 1 "https://github.com/${REPO}.git" "${TMP_DIR}/miseban-ai"
            cd "${TMP_DIR}/miseban-ai"
            docker build -f Dockerfile.agent -t "${DOCKER_IMAGE}" .
            rm -rf "${TMP_DIR}"
        fi
    fi
    echo ""

    # Run the container
    echo -e "${CYAN}Starting miseban-agent container...${NC}"
    docker run -d \
        --name miseban-agent \
        --restart unless-stopped \
        --network host \
        -v "${CONFIG_DIR}:/root/.miseban:ro" \
        -v "/tmp/miseban:/tmp/miseban" \
        "${DOCKER_IMAGE}"

    echo ""
    echo -e "${GREEN}Container started!${NC}"
    echo ""

    # Print status
    echo "  Container: $(docker ps --filter name=miseban-agent --format '{{.Status}}')"
    echo "  Logs:      docker logs -f miseban-agent"
    echo "  Stop:      docker stop miseban-agent"
    echo "  Restart:   docker restart miseban-agent"

# ---------------------------------------------------------------------------
# 7B. Binary install (fallback)
# ---------------------------------------------------------------------------
else
    echo -e "${BOLD}Step 4: Installing Binary${NC}"
    echo ""

    # Install ffmpeg if missing
    if ! command -v ffmpeg &>/dev/null; then
        echo -e "${YELLOW}ffmpeg not found. Installing...${NC}"
        if command -v apt-get &>/dev/null; then
            sudo apt-get update -qq && sudo apt-get install -y -qq ffmpeg
        elif command -v brew &>/dev/null; then
            brew install ffmpeg
        elif command -v pacman &>/dev/null; then
            sudo pacman -S --noconfirm ffmpeg
        else
            echo -e "${RED}Please install ffmpeg manually: https://ffmpeg.org/download.html${NC}"
            exit 1
        fi
    fi
    echo -e "${GREEN}ffmpeg: $(ffmpeg -version 2>&1 | head -1)${NC}"

    # Download binary
    case "$OS" in
        linux)  TARGET="${ARCH}-unknown-linux-gnu" ;;
        darwin) TARGET="${ARCH}-apple-darwin" ;;
    esac

    echo -e "${CYAN}Downloading miseban-agent (${TARGET})...${NC}"
    LATEST_URL="https://github.com/${REPO}/releases/latest/download/${BINARY}-${TARGET}"

    if curl -fsSL -o "/tmp/${BINARY}" "${LATEST_URL}" 2>/dev/null; then
        chmod +x "/tmp/${BINARY}"
        sudo mv "/tmp/${BINARY}" "${INSTALL_DIR}/${BINARY}"
        echo -e "${GREEN}Installed ${BINARY} to ${INSTALL_DIR}${NC}"
    else
        echo -e "${YELLOW}Pre-built binary not found. Building from source...${NC}"
        if ! command -v cargo &>/dev/null; then
            echo -e "${CYAN}Installing Rust...${NC}"
            curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
            source "$HOME/.cargo/env"
        fi

        TMP_DIR=$(mktemp -d)
        git clone --depth 1 "https://github.com/${REPO}.git" "${TMP_DIR}/miseban-ai"
        cd "${TMP_DIR}/miseban-ai"
        cargo build --release --package agent
        sudo cp "target/release/${BINARY}" "${INSTALL_DIR}/${BINARY}"
        rm -rf "${TMP_DIR}"
        echo -e "${GREEN}Built and installed ${BINARY}${NC}"
    fi

    # Install systemd service (Linux only)
    if [[ "$OS" == "linux" ]] && command -v systemctl &>/dev/null; then
        echo -e "${CYAN}Installing systemd service...${NC}"
        sudo tee /etc/systemd/system/${SERVICE_NAME}.service > /dev/null <<UNIT
[Unit]
Description=MisebanAI Camera Agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$(whoami)
ExecStart=${INSTALL_DIR}/${BINARY}
Restart=always
RestartSec=10
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
UNIT

        sudo systemctl daemon-reload
        sudo systemctl enable ${SERVICE_NAME}
        sudo systemctl start ${SERVICE_NAME}
        echo -e "${GREEN}Service installed and started!${NC}"
    fi

    # macOS: start agent now
    if [[ "$OS" == "darwin" ]]; then
        echo -e "${CYAN}Starting agent...${NC}"
        nohup ${INSTALL_DIR}/${BINARY} > /tmp/miseban-agent.log 2>&1 &
        AGENT_PID=$!
        echo -e "${GREEN}Agent started (PID: ${AGENT_PID})${NC}"
        echo "  Logs: tail -f /tmp/miseban-agent.log"
    fi
fi

# ---------------------------------------------------------------------------
# 8. Done!
# ---------------------------------------------------------------------------
echo ""
echo "  ╔══════════════════════════════════════════╗"
echo "  ║          インストール完了!                ║"
echo "  ╚══════════════════════════════════════════╝"
echo ""
echo -e "  ${GREEN}ダッシュボード: https://misebanai.com/dashboard/${NC}"
echo ""
echo "  設定ファイル: ${CONFIG_DIR}/config.toml"
echo "  カメラURL:   ${CAMERA_URL}"
echo ""
echo "  設定を変更するには:"
echo -e "    ${CYAN}nano ${CONFIG_DIR}/config.toml${NC}"
echo ""
echo "  カメラ再検索:"
echo -e "    ${CYAN}./scripts/discover-cameras.sh${NC}"
echo ""
