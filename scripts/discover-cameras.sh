#!/bin/sh
# ============================================================================
# MisebanAI — Camera Discovery Script
# Scans the local network for IP cameras (RTSP/HTTP) on macOS and Linux.
#
# Usage:
#   ./scripts/discover-cameras.sh
#   ./scripts/discover-cameras.sh --json    # Output as JSON array
#
# Requirements:
#   - arp or nmap (arp is available on macOS by default)
#   - nc (netcat, available on macOS by default)
# ============================================================================
set -eu

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------
RTSP_PORTS="554 8554"
HTTP_PORTS="80 8080 8000 8888"
CONNECT_TIMEOUT=1
JSON_MODE=false

# Brand-specific RTSP paths (most common first)
RTSP_PATHS="
/stream1
/Streaming/Channels/101
/h264Preview_01_main
/cam/realmonitor?channel=1&subtype=0
/live/ch00_0
/videoMain
/live
/ch0_0.h264
/unicast
/
"

# Brand-specific HTTP snapshot paths
HTTP_PATHS="
/snap.jpg
/snapshot.jpg
/cgi-bin/snapshot.cgi
/image/jpeg.cgi
/jpg/image.jpg
/capture
/tmpfs/auto.jpg
/ISAPI/Streaming/channels/101/picture
"

# ---------------------------------------------------------------------------
# Colors (only when stdout is a terminal)
# ---------------------------------------------------------------------------
if [ -t 1 ]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    CYAN='\033[0;36m'
    BOLD='\033[1m'
    NC='\033[0m'
else
    RED='' GREEN='' YELLOW='' CYAN='' BOLD='' NC=''
fi

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
log()   { printf "%b\n" "$*" >&2; }
info()  { log "${CYAN}[INFO]${NC} $*"; }
ok()    { log "${GREEN}[OK]${NC}   $*"; }
warn()  { log "${YELLOW}[WARN]${NC} $*"; }

# Check if a TCP port is open (returns 0 = open, 1 = closed/timeout)
check_port() {
    _ip="$1"
    _port="$2"
    if command -v nc >/dev/null 2>&1; then
        nc -z -w "$CONNECT_TIMEOUT" "$_ip" "$_port" >/dev/null 2>&1
    elif command -v bash >/dev/null 2>&1; then
        # Fallback: bash /dev/tcp (not POSIX, but common)
        bash -c "echo >/dev/tcp/$_ip/$_port" 2>/dev/null
    else
        return 1
    fi
}

# Get local IP address (works on macOS and Linux)
get_local_ip() {
    if command -v ipconfig >/dev/null 2>&1 && [ "$(uname)" = "Darwin" ]; then
        # macOS: try en0 (Wi-Fi) first, then en1
        for iface in en0 en1 en2; do
            _ip=$(ipconfig getifaddr "$iface" 2>/dev/null) && [ -n "$_ip" ] && echo "$_ip" && return 0
        done
    fi
    # Linux / fallback: route + ip/ifconfig
    if command -v ip >/dev/null 2>&1; then
        ip route get 8.8.8.8 2>/dev/null | sed -n 's/.*src \([0-9.]*\).*/\1/p' | head -1
    elif command -v ifconfig >/dev/null 2>&1; then
        ifconfig | sed -n 's/.*inet \([0-9]*\.[0-9]*\.[0-9]*\.[0-9]*\).*/\1/p' | grep -v '127.0.0.1' | head -1
    fi
}

# Get subnet prefix (first 3 octets) from an IP
get_subnet() {
    echo "$1" | sed 's/\.[0-9]*$//'
}

# Discover IPs on the local network via ARP table
discover_hosts() {
    _found=""
    # Method 1: arp -a (available on macOS and most Linux)
    if command -v arp >/dev/null 2>&1; then
        _found=$(arp -a 2>/dev/null | sed -n 's/.*(\([0-9]*\.[0-9]*\.[0-9]*\.[0-9]*\)).*/\1/p' | sort -t. -k4 -n -u)
    fi

    # Method 2: nmap ping scan (if arp returned nothing or nmap is available)
    if [ -z "$_found" ] && command -v nmap >/dev/null 2>&1; then
        _subnet=$(get_subnet "$1")
        _found=$(nmap -sn "${_subnet}.0/24" 2>/dev/null | sed -n 's/.*scan report for[^(]*(\{0,1\}\([0-9]*\.[0-9]*\.[0-9]*\.[0-9]*\)).*/\1/p' | sort -t. -k4 -n -u)
    fi

    # Filter to same subnet only
    if [ -n "$_found" ]; then
        _subnet=$(get_subnet "$1")
        echo "$_found" | while IFS= read -r _ip; do
            _ip_subnet=$(get_subnet "$_ip")
            if [ "$_ip_subnet" = "$_subnet" ] && [ "$_ip" != "$1" ]; then
                echo "$_ip"
            fi
        done
    fi
}

# Brand hint from RTSP port + path
guess_brand() {
    _path="$1"
    case "$_path" in
        */Streaming/Channels/*)   echo "Hikvision/EZVIZ" ;;
        */h264Preview_01_main*)   echo "Reolink" ;;
        */cam/realmonitor*)       echo "Dahua" ;;
        */stream1*)               echo "TP-Link TAPO" ;;
        */live/ch00_0*)           echo "Hanwha/Samsung" ;;
        */unicast*)               echo "SwitchBot" ;;
        */videoMain*)             echo "Axis" ;;
        *)                        echo "Generic" ;;
    esac
}

# ---------------------------------------------------------------------------
# Parse arguments
# ---------------------------------------------------------------------------
for arg in "$@"; do
    case "$arg" in
        --json) JSON_MODE=true ;;
        --help|-h)
            log "Usage: $0 [--json] [--help]"
            log ""
            log "Scans the local network for IP cameras."
            log "  --json   Output results as JSON array"
            log "  --help   Show this help"
            exit 0
            ;;
    esac
done

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
log ""
log "  ${BOLD}MisebanAI Camera Discovery${NC}"
log "  ========================="
log ""

LOCAL_IP=$(get_local_ip)
if [ -z "$LOCAL_IP" ]; then
    log "${RED}ERROR: Could not determine local IP address.${NC}"
    log "Make sure you are connected to a Wi-Fi or Ethernet network."
    exit 1
fi
SUBNET=$(get_subnet "$LOCAL_IP")
info "Local IP: ${BOLD}$LOCAL_IP${NC}"
info "Scanning subnet: ${BOLD}${SUBNET}.0/24${NC}"
log ""

# Step 1: Discover hosts on the network
info "Step 1/3: Discovering devices on the network..."
HOSTS=$(discover_hosts "$LOCAL_IP")
HOST_COUNT=$(echo "$HOSTS" | grep -c '[0-9]' || true)

if [ "$HOST_COUNT" -eq 0 ]; then
    warn "No devices found via ARP table."
    warn "Falling back to sequential scan of ${SUBNET}.1-254 (this may take a minute)..."
    HOSTS=""
    _i=1
    while [ "$_i" -le 254 ]; do
        _target="${SUBNET}.${_i}"
        if [ "$_target" != "$LOCAL_IP" ]; then
            HOSTS="${HOSTS}${_target}
"
        fi
        _i=$((_i + 1))
    done
    HOST_COUNT=254
fi
info "Found ${BOLD}${HOST_COUNT}${NC} device(s) to probe"
log ""

# Step 2: Check RTSP and HTTP ports
info "Step 2/3: Probing camera ports (RTSP: $RTSP_PORTS / HTTP: $HTTP_PORTS)..."

CAMERA_IPS=""
CAMERA_DETAILS=""
FOUND_COUNT=0

echo "$HOSTS" | while IFS= read -r ip; do
    [ -z "$ip" ] && continue

    # Check RTSP ports
    for port in $RTSP_PORTS; do
        if check_port "$ip" "$port"; then
            ok "RTSP port open: ${BOLD}${ip}:${port}${NC}"
            echo "rtsp|${ip}|${port}" >> /tmp/miseban_discovery_$$
        fi
    done

    # Check HTTP ports
    for port in $HTTP_PORTS; do
        if check_port "$ip" "$port"; then
            # Don't report port 80 on every device (routers, etc.) - only if RTSP is also open
            # or if it's a non-standard port
            if [ "$port" != "80" ] || grep -q "rtsp|${ip}|" /tmp/miseban_discovery_$$ 2>/dev/null; then
                ok "HTTP port open: ${BOLD}${ip}:${port}${NC}"
                echo "http|${ip}|${port}" >> /tmp/miseban_discovery_$$
            fi
        fi
    done
done

# Read results
RESULTS_FILE="/tmp/miseban_discovery_$$"
if [ ! -f "$RESULTS_FILE" ]; then
    log ""
    warn "No cameras found on the network."
    log ""
    log "  ${BOLD}Troubleshooting:${NC}"
    log "  1. Make sure your camera is powered on and connected to Wi-Fi/Ethernet"
    log "  2. Check that your camera is on the same network as this computer"
    log "  3. Some cameras need RTSP enabled in their app settings"
    log "     - TAPO: TAPO app > Camera Settings > Advanced > RTSP"
    log "     - Reolink: Reolink app > Device Settings > Network > Advanced > Port"
    log "     - SwitchBot: SwitchBot app > Camera > Settings > RTSP"
    log "  4. Try installing nmap for more thorough scanning:"
    log "     ${CYAN}brew install nmap${NC} (macOS) or ${CYAN}sudo apt install nmap${NC} (Linux)"
    log ""
    if [ "$JSON_MODE" = "true" ]; then
        echo "[]"
    fi
    exit 0
fi

log ""

# Step 3: Build camera URL suggestions
info "Step 3/3: Building RTSP URL suggestions..."
log ""

CAMERA_NUM=0
JSON_OUTPUT="["
FIRST_JSON=true

while IFS='|' read -r proto ip port; do
    [ -z "$proto" ] && continue

    if [ "$proto" = "rtsp" ]; then
        # Suggest URLs for all known brands
        for path in $RTSP_PATHS; do
            [ -z "$path" ] && continue
            CAMERA_NUM=$((CAMERA_NUM + 1))
            brand=$(guess_brand "$path")
            url="rtsp://${ip}:${port}${path}"

            if [ "$JSON_MODE" = "true" ]; then
                if [ "$FIRST_JSON" = "true" ]; then FIRST_JSON=false; else JSON_OUTPUT="${JSON_OUTPUT},"; fi
                JSON_OUTPUT="${JSON_OUTPUT}{\"ip\":\"${ip}\",\"port\":${port},\"protocol\":\"rtsp\",\"url\":\"${url}\",\"brand\":\"${brand}\",\"path\":\"${path}\"}"
            else
                log "  ${GREEN}[$CAMERA_NUM]${NC} ${BOLD}${brand}${NC}"
                log "      ${CYAN}${url}${NC}"
                log "      (credentials may be needed: rtsp://user:pass@${ip}:${port}${path})"
                log ""
            fi
        done
    elif [ "$proto" = "http" ]; then
        for path in $HTTP_PATHS; do
            [ -z "$path" ] && continue
            CAMERA_NUM=$((CAMERA_NUM + 1))
            url="http://${ip}:${port}${path}"

            if [ "$JSON_MODE" = "true" ]; then
                if [ "$FIRST_JSON" = "true" ]; then FIRST_JSON=false; else JSON_OUTPUT="${JSON_OUTPUT},"; fi
                JSON_OUTPUT="${JSON_OUTPUT}{\"ip\":\"${ip}\",\"port\":${port},\"protocol\":\"http\",\"url\":\"${url}\",\"brand\":\"Generic\",\"path\":\"${path}\"}"
            else
                log "  ${GREEN}[$CAMERA_NUM]${NC} ${BOLD}HTTP Snapshot${NC}"
                log "      ${CYAN}${url}${NC}"
                log ""
            fi
        done
    fi
done < "$RESULTS_FILE"

JSON_OUTPUT="${JSON_OUTPUT}]"

# Cleanup temp file
rm -f "$RESULTS_FILE"

if [ "$JSON_MODE" = "true" ]; then
    echo "$JSON_OUTPUT"
    exit 0
fi

log "  ========================================================"
log ""
log "  ${BOLD}Next steps:${NC}"
log ""
log "  1. Find your camera brand above and copy the RTSP URL"
log "  2. If your camera has a username/password, add them:"
log "     ${CYAN}rtsp://USERNAME:PASSWORD@IP:PORT/path${NC}"
log ""
log "  ${BOLD}Common default credentials:${NC}"
log "     Hikvision : admin / (set during setup)"
log "     Dahua     : admin / admin"
log "     Reolink   : admin / (set during setup)"
log "     TAPO      : (set in TAPO app > Camera > Advanced > Camera Account)"
log "     SwitchBot : (no auth needed for RTSP)"
log ""
log "  3. Test the URL with ffplay or VLC:"
log "     ${CYAN}ffplay rtsp://admin:pass@192.168.1.100:554/stream1${NC}"
log ""
log "  4. Start MisebanAI:"
log "     ${CYAN}docker run -d miseban/agent --camera 'rtsp://...' --token YOUR_TOKEN${NC}"
log ""
log "  Or run the install script:"
log "     ${CYAN}./scripts/install.sh${NC}"
log ""
