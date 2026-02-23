#!/usr/bin/env bash
# ============================================================================
# MisebanAI Camera Agent — One-command installer
# Usage: curl -fsSL https://misebanai.com/install.sh | bash
# ============================================================================
set -euo pipefail

REPO="yukihamada/miseban-ai"
BINARY="miseban-agent"
INSTALL_DIR="/usr/local/bin"
CONFIG_DIR="$HOME/.miseban"
SERVICE_NAME="miseban-agent"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
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

case "$OS" in
    linux)  TARGET="${ARCH}-unknown-linux-gnu" ;;
    darwin) TARGET="${ARCH}-apple-darwin" ;;
    *)
        echo -e "${RED}Unsupported OS: $OS${NC}"
        exit 1
        ;;
esac

echo -e "${CYAN}Platform: ${OS}/${ARCH} (${TARGET})${NC}"

# ---------------------------------------------------------------------------
# 2. Install ffmpeg if missing
# ---------------------------------------------------------------------------
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

# ---------------------------------------------------------------------------
# 3. Download binary
# ---------------------------------------------------------------------------
echo -e "${CYAN}Downloading miseban-agent...${NC}"

LATEST_URL="https://github.com/${REPO}/releases/latest/download/${BINARY}-${TARGET}"

# Try GitHub release first; fallback to building from source.
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

# ---------------------------------------------------------------------------
# 4. Create config directory
# ---------------------------------------------------------------------------
mkdir -p "${CONFIG_DIR}"
echo -e "${GREEN}Config directory: ${CONFIG_DIR}${NC}"

# ---------------------------------------------------------------------------
# 5. Install systemd service (Linux only)
# ---------------------------------------------------------------------------
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

# ---------------------------------------------------------------------------
# 6. Done!
# ---------------------------------------------------------------------------
echo ""
echo "  ╔══════════════════════════════════════════╗"
echo "  ║          インストール完了!                ║"
echo "  ╚══════════════════════════════════════════╝"
echo ""
echo -e "  ${GREEN}ブラウザで http://localhost:3939 を開いてセットアップしてください${NC}"
echo ""
echo "  使い方:"
echo "    miseban-agent              # 自動モード（設定なし→セットアップ起動）"
echo "    miseban-agent --setup      # セットアップウィザード強制起動"
echo "    miseban-agent --dry-run    # テストモード（アップロードしない）"
echo ""

# Auto-start setup if no config exists yet.
if [[ ! -f "${CONFIG_DIR}/config.toml" ]]; then
    echo -e "${YELLOW}設定ファイルが未作成のため、エージェントがセットアップモードで起動します...${NC}"
    echo ""
    ${INSTALL_DIR}/${BINARY}
fi
