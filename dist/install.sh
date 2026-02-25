#!/bin/bash
# MisebanAI Camera Agent — Raspberry Pi installer
# Usage: curl -fsSL https://misebanai.com/install.sh | bash
set -euo pipefail

INSTALL_DIR="/usr/local/bin"
CONFIG_DIR="$HOME/.miseban"
SERVICE_NAME="miseban-agent"

echo "=== MisebanAI Camera Agent Installer ==="
echo ""

# Check architecture
ARCH=$(uname -m)
case "$ARCH" in
  aarch64|arm64) ARCH_LABEL="arm64" ;;
  armv7l|armv6l) echo "Error: 32-bit ARM is not supported. Use Raspberry Pi 4/5 with 64-bit OS."; exit 1 ;;
  *) echo "Error: Unsupported architecture: $ARCH"; exit 1 ;;
esac

# Check if running on Linux
if [ "$(uname -s)" != "Linux" ]; then
  echo "Error: This installer is for Linux (Raspberry Pi). Detected: $(uname -s)"
  exit 1
fi

echo "[1/4] Installing dependencies..."
if command -v apt-get &>/dev/null; then
  sudo apt-get update -qq
  sudo apt-get install -y -qq ffmpeg ca-certificates
elif command -v dnf &>/dev/null; then
  sudo dnf install -y ffmpeg ca-certificates
else
  echo "Warning: Could not install dependencies automatically. Please install ffmpeg manually."
fi

echo "[2/4] Installing miseban-agent binary..."
if [ -f "./miseban-agent" ]; then
  # Local install from tarball
  sudo cp ./miseban-agent "$INSTALL_DIR/miseban-agent"
else
  echo "Error: miseban-agent binary not found. Extract the tarball first:"
  echo "  tar xzf miseban-agent-arm64.tar.gz && cd miseban-agent && ./install.sh"
  exit 1
fi
sudo chmod +x "$INSTALL_DIR/miseban-agent"

echo "[3/4] Creating config directory..."
mkdir -p "$CONFIG_DIR"

echo "[4/4] Installing systemd service..."
sudo tee /etc/systemd/system/${SERVICE_NAME}.service >/dev/null <<UNIT
[Unit]
Description=MisebanAI Camera Agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=${INSTALL_DIR}/miseban-agent
Restart=always
RestartSec=10
User=$(whoami)
Environment=HOME=$HOME
WorkingDirectory=$HOME

[Install]
WantedBy=multi-user.target
UNIT

sudo systemctl daemon-reload
sudo systemctl enable ${SERVICE_NAME}
sudo systemctl start ${SERVICE_NAME}

echo ""
echo "=== Installation complete ==="
echo ""
echo "MisebanAI Camera Agent is running!"
echo ""
echo "  Setup wizard: http://$(hostname -I | awk '{print $1}'):3939"
echo "  Status:       sudo systemctl status ${SERVICE_NAME}"
echo "  Logs:         sudo journalctl -u ${SERVICE_NAME} -f"
echo "  Config:       ${CONFIG_DIR}/config.toml"
echo ""
echo "Open the setup wizard URL in your browser to pair this device."
