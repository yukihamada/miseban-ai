#!/bin/sh
# Download YOLOv8n ONNX model (opset 17) if not present.
# API starts even if download fails; analysis returns 503 without counting.
MODEL_PATH="${MISEBAN_MODEL_PATH:-/models/yolov8n.onnx}"

if [ ! -f "$MODEL_PATH" ]; then
  mkdir -p "$(dirname "$MODEL_PATH")"
  echo "[entrypoint] Downloading YOLOv8n ONNX model..."

  # Source 1: GitHub release (YOLOv8n opset 17, ONNX Runtime compatible)
  curl -fsSL --max-time 120 \
    "https://github.com/yukihamada/miseban-ai/releases/download/v0.1.0-models/yolov8n.onnx" \
    -o "$MODEL_PATH" 2>/dev/null && \
    echo "[entrypoint] Model downloaded ($(wc -c < "$MODEL_PATH") bytes)" && \
    exec miseban-api

  echo "[entrypoint] Model download failed — AI inference unavailable (503; frames are not counted)"
  rm -f "$MODEL_PATH"
else
  echo "[entrypoint] Using cached model at $MODEL_PATH ($(wc -c < "$MODEL_PATH") bytes)"
fi

exec miseban-api
