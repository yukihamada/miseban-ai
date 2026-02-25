#!/usr/bin/env bash
# ============================================================================
# Download YOLOv8n ONNX model for MisebanAI inference
#
# Usage:
#   ./scripts/download-model.sh [--output DIR]
#
# Strategy:
#   1. Try to export from .pt using the `ultralytics` Python package (best)
#   2. Fall back to downloading the .pt and converting via Python one-liner
#   3. Fall back to a pre-exported ONNX from a known-good source
#
# Default output directory: models/
# ============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Default output directory
OUTPUT_DIR="${PROJECT_ROOT}/models"

# Parse arguments
while [[ $# -gt 0 ]]; do
    case "$1" in
        --output)
            OUTPUT_DIR="$2"
            shift 2
            ;;
        -h|--help)
            echo "Usage: $0 [--output DIR]"
            echo ""
            echo "Downloads the YOLOv8n ONNX model for person detection."
            echo ""
            echo "Options:"
            echo "  --output DIR  Output directory (default: models/)"
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

MODEL_FILE="${OUTPUT_DIR}/yolov8n.onnx"

# Source URLs
PT_URL="https://github.com/ultralytics/assets/releases/download/v8.3.0/yolov8n.pt"

echo ""
echo "  MisebanAI — YOLOv8n ONNX Model Downloader"
echo "  ─────────────────────────────────────────"
echo ""

# Check if model already exists and is valid
if [ -f "${MODEL_FILE}" ]; then
    FILE_SIZE=$(stat -f%z "${MODEL_FILE}" 2>/dev/null || stat -c%s "${MODEL_FILE}" 2>/dev/null || echo "0")
    # YOLOv8n ONNX should be ~12-13MB
    if [ "${FILE_SIZE}" -gt 5000000 ]; then
        echo "  Model already exists: ${MODEL_FILE} ($(( FILE_SIZE / 1024 / 1024 ))MB)"
        echo "  To re-download, delete it first: rm ${MODEL_FILE}"
        echo ""
        exit 0
    else
        echo "  Existing model file is too small (${FILE_SIZE} bytes), re-downloading..."
        rm -f "${MODEL_FILE}"
    fi
fi

# Create output directory
mkdir -p "${OUTPUT_DIR}"

# ---------------------------------------------------------------------------
# Strategy 1: Use ultralytics Python package to download + export
# ---------------------------------------------------------------------------
try_ultralytics_export() {
    if ! command -v python3 &>/dev/null; then
        return 1
    fi

    # Check if ultralytics is installed
    if ! python3 -c "import ultralytics" 2>/dev/null; then
        echo "  Installing ultralytics Python package..."
        pip3 install --quiet ultralytics 2>/dev/null || return 1
    fi

    echo "  Exporting YOLOv8n to ONNX via ultralytics..."
    python3 -c "
from ultralytics import YOLO
model = YOLO('yolov8n.pt')
model.export(format='onnx', imgsz=640, simplify=True)
" 2>/dev/null || return 1

    # The export creates yolov8n.onnx in the current directory
    if [ -f "yolov8n.onnx" ]; then
        mv yolov8n.onnx "${MODEL_FILE}"
        # Clean up .pt if downloaded
        rm -f yolov8n.pt
        return 0
    fi

    return 1
}

# ---------------------------------------------------------------------------
# Strategy 2: Download .pt then convert with Python one-liner
# ---------------------------------------------------------------------------
try_pt_to_onnx() {
    if ! command -v python3 &>/dev/null; then
        return 1
    fi

    local PT_FILE="${OUTPUT_DIR}/yolov8n.pt"

    echo "  Downloading YOLOv8n .pt from Ultralytics..."
    if command -v curl &>/dev/null; then
        curl -fSL --progress-bar -o "${PT_FILE}" "${PT_URL}" || return 1
    elif command -v wget &>/dev/null; then
        wget -q --show-progress -O "${PT_FILE}" "${PT_URL}" || return 1
    else
        return 1
    fi

    echo "  Converting .pt to .onnx..."
    pip3 install --quiet ultralytics 2>/dev/null || true

    python3 -c "
from ultralytics import YOLO
import sys
model = YOLO('${PT_FILE}')
model.export(format='onnx', imgsz=640, simplify=True)
" 2>/dev/null || { rm -f "${PT_FILE}"; return 1; }

    # ultralytics puts the .onnx next to the .pt
    local EXPORTED="${OUTPUT_DIR}/yolov8n.onnx"
    if [ ! -f "${EXPORTED}" ]; then
        # It might have been placed in the current working directory
        if [ -f "yolov8n.onnx" ]; then
            mv "yolov8n.onnx" "${EXPORTED}"
        fi
    fi

    # Clean up .pt
    rm -f "${PT_FILE}"

    if [ -f "${EXPORTED}" ]; then
        return 0
    fi

    return 1
}

# ---------------------------------------------------------------------------
# Strategy 3: Download pre-exported ONNX from PyTorch/ONNX model zoo or
#             from the Ultralytics .pt and ship without conversion
# ---------------------------------------------------------------------------
try_direct_download() {
    # Download the .pt file and keep it — the init_model() will need to be
    # updated to support .pt if this is the only fallback.
    # For now, try the .pt file since Ultralytics hosts it reliably.
    echo "  Downloading YOLOv8n .pt from Ultralytics GitHub releases..."
    echo "  URL: ${PT_URL}"

    local PT_FILE="${OUTPUT_DIR}/yolov8n.pt"

    if command -v curl &>/dev/null; then
        curl -fSL --progress-bar -o "${PT_FILE}" "${PT_URL}" || return 1
    elif command -v wget &>/dev/null; then
        wget -q --show-progress -O "${PT_FILE}" "${PT_URL}" || return 1
    else
        echo "  ERROR: Neither curl nor wget found."
        return 1
    fi

    if [ ! -f "${PT_FILE}" ]; then
        return 1
    fi

    local FILE_SIZE
    FILE_SIZE=$(stat -f%z "${PT_FILE}" 2>/dev/null || stat -c%s "${PT_FILE}" 2>/dev/null || echo "0")
    if [ "${FILE_SIZE}" -lt 3000000 ]; then
        echo "  ERROR: Downloaded .pt file is too small (${FILE_SIZE} bytes)."
        rm -f "${PT_FILE}"
        return 1
    fi

    echo ""
    echo "  Downloaded .pt file: ${PT_FILE} ($(( FILE_SIZE / 1024 / 1024 ))MB)"
    echo ""
    echo "  NOTE: The .pt file must be converted to ONNX format."
    echo "  Install Python + ultralytics, then run:"
    echo "    pip install ultralytics"
    echo "    python3 -c \"from ultralytics import YOLO; YOLO('${PT_FILE}').export(format='onnx')\""
    echo "    mv ${OUTPUT_DIR}/yolov8n.onnx ${MODEL_FILE}"
    echo ""
    return 1
}

# ---------------------------------------------------------------------------
# Execute strategies in order
# ---------------------------------------------------------------------------

echo "  Strategy 1: Export via ultralytics Python package..."
if try_ultralytics_export; then
    echo ""
else
    echo "  Strategy 1 unavailable (Python/ultralytics not found)."
    echo ""
    echo "  Strategy 2: Download .pt then convert via ultralytics..."
    if try_pt_to_onnx; then
        echo ""
    else
        echo "  Strategy 2 failed (Python/ultralytics not available)."
        echo ""
        echo "  Strategy 3: Download .pt for manual conversion..."
        try_direct_download
    fi
fi

# Verify final result
if [ -f "${MODEL_FILE}" ]; then
    FILE_SIZE=$(stat -f%z "${MODEL_FILE}" 2>/dev/null || stat -c%s "${MODEL_FILE}" 2>/dev/null || echo "0")
    echo "  Download complete: ${MODEL_FILE} ($(( FILE_SIZE / 1024 / 1024 ))MB)"
    echo ""
    echo "  To use with the API server:"
    echo "    export MISEBAN_MODEL_PATH=${MODEL_FILE}"
    echo "    cargo run -p api"
    echo ""
    echo "  Default path: models/yolov8n.onnx"
    echo ""
    exit 0
else
    echo ""
    echo "  WARNING: ONNX model not created automatically."
    echo "  The inference code will fall back to no-op mode (people_count = 0)."
    echo ""
    echo "  To create the ONNX model manually:"
    echo "    pip install ultralytics"
    echo "    python3 -c \"from ultralytics import YOLO; YOLO('yolov8n.pt').export(format='onnx')\""
    echo "    mv yolov8n.onnx ${MODEL_FILE}"
    echo ""
    # Exit 0 even on failure: the Rust code gracefully handles missing model
    exit 0
fi
