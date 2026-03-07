#!/bin/sh
# MisebanAI Camera Agent — Universal Installer
# Usage: curl -fsSL https://misebanai.com/install.sh | sh
set -e

REPO="yukihamada/miseban-ai"
INSTALL_DIR="/usr/local/bin"
SERVICE_NAME="miseban-agent"

echo "=== MisebanAI Camera Agent Installer ==="
echo ""

# Detect OS and architecture
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)
    case "$ARCH" in
      x86_64)           ARTIFACT="miseban-agent-linux-x86_64.tar.gz" ;;
      aarch64|arm64)    ARTIFACT="miseban-agent-linux-arm64.tar.gz" ;;
      armv7l|armv6l)    ARTIFACT="miseban-agent-linux-armv7.tar.gz" ;;
      *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
    esac
    ;;
  Darwin)
    ARTIFACT="miseban-agent-macos-universal.tar.gz"
    ;;
  *)
    echo "Unsupported OS: $OS"
    echo "Windows: download miseban-agent-windows-x86_64.zip from https://github.com/$REPO/releases"
    exit 1
    ;;
esac

# Get latest release tag
echo "[1/4] Fetching latest version..."
LATEST=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | grep '"tag_name"' | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')
if [ -z "$LATEST" ]; then
  echo "Could not determine latest version. Set MISEBAN_VERSION env var to override."
  LATEST="${MISEBAN_VERSION:-v1.0.0}"
fi
echo "    Version: $LATEST"

# Install ffmpeg if missing
echo "[2/4] Checking dependencies..."
if ! command -v ffmpeg >/dev/null 2>&1; then
  echo "    Installing ffmpeg..."
  if command -v apt-get >/dev/null 2>&1; then
    sudo apt-get install -y -qq ffmpeg ca-certificates
  elif command -v brew >/dev/null 2>&1; then
    brew install ffmpeg
  elif command -v dnf >/dev/null 2>&1; then
    sudo dnf install -y ffmpeg
  elif command -v yum >/dev/null 2>&1; then
    sudo yum install -y ffmpeg
  else
    echo "    WARNING: Could not install ffmpeg automatically. Please install it: https://ffmpeg.org/download.html"
  fi
else
  echo "    ffmpeg: OK ($(ffmpeg -version 2>&1 | head -1 | cut -d' ' -f3))"
fi

# Download binary
echo "[3/4] Downloading $ARTIFACT..."
URL="https://github.com/$REPO/releases/download/$LATEST/$ARTIFACT"
TMPDIR=$(mktemp -d)
curl -fsSL "$URL" -o "$TMPDIR/$ARTIFACT"
tar -xzf "$TMPDIR/$ARTIFACT" -C "$TMPDIR"

# Install binary
echo "[4/4] Installing to $INSTALL_DIR..."
if [ -w "$INSTALL_DIR" ]; then
  cp "$TMPDIR/miseban-agent" "$INSTALL_DIR/miseban-agent"
else
  sudo cp "$TMPDIR/miseban-agent" "$INSTALL_DIR/miseban-agent"
fi
chmod +x "$INSTALL_DIR/miseban-agent" 2>/dev/null || sudo chmod +x "$INSTALL_DIR/miseban-agent"
rm -rf "$TMPDIR"

# Linux: install systemd service
if [ "$OS" = "Linux" ] && command -v systemctl >/dev/null 2>&1; then
  echo "    Installing systemd service..."
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
  sudo systemctl start ${SERVICE_NAME} || true
fi

# macOS: install launchd plist
if [ "$OS" = "Darwin" ]; then
  PLIST="$HOME/Library/LaunchAgents/com.misebanai.agent.plist"
  mkdir -p "$(dirname "$PLIST")"
  cat > "$PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.misebanai.agent</string>
  <key>ProgramArguments</key>
  <array><string>${INSTALL_DIR}/miseban-agent</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$HOME/.miseban/agent.log</string>
  <key>StandardErrorPath</key><string>$HOME/.miseban/agent.log</string>
</dict>
</plist>
PLIST
  mkdir -p "$HOME/.miseban"
  launchctl load "$PLIST" 2>/dev/null || true
fi

echo ""
echo "=== インストール完了 / Installation complete ==="
echo ""
echo "  セットアップ画面 / Setup wizard:"
if [ "$OS" = "Linux" ]; then
  IP=$(hostname -I 2>/dev/null | awk '{print $1}' || echo "localhost")
  echo "    http://${IP}:3939"
elif [ "$OS" = "Darwin" ]; then
  echo "    http://localhost:3939"
fi
echo ""
echo "  Version: $LATEST"
echo "  Binary:  $INSTALL_DIR/miseban-agent"
echo ""
echo "  ブラウザでセットアップ画面を開いてカメラを接続してください。"
