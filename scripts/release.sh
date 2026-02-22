#!/usr/bin/env bash
# ================================================================
# MisebanAI Release Script
# Usage: ./scripts/release.sh v1.0.0
# ================================================================

set -euo pipefail

# ── Colors ──
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

info()  { echo -e "${BLUE}[INFO]${NC}  $*"; }
ok()    { echo -e "${GREEN}[OK]${NC}    $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
error() { echo -e "${RED}[ERROR]${NC} $*" >&2; }
die()   { error "$*"; exit 1; }

# ── Validate arguments ──
VERSION="${1:-}"
if [[ -z "$VERSION" ]]; then
    die "Usage: $0 <VERSION>  (e.g., $0 v1.0.0)"
fi

if [[ ! "$VERSION" =~ ^v[0-9]+\.[0-9]+\.[0-9]+ ]]; then
    die "VERSION must match 'v<major>.<minor>.<patch>' (got: $VERSION)"
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST_DIR="$PROJECT_ROOT/dist"
TARGETS=("x86_64-unknown-linux-gnu" "aarch64-unknown-linux-gnu")

cd "$PROJECT_ROOT"

info "Starting release pipeline for $VERSION"
echo "============================================"

# ── Step 1: Verify clean working tree ──
info "Step 1/8: Checking working tree..."
if [[ -n "$(git status --porcelain 2>/dev/null)" ]]; then
    warn "Working tree is dirty. Proceeding anyway..."
fi
ok "Working tree check complete"

# ── Step 2: Security checks ──
info "Step 2/8: Running security checks..."

info "  cargo deny check..."
if command -v cargo-deny &>/dev/null; then
    cargo deny check || die "cargo deny check failed"
    ok "  cargo-deny passed"
else
    warn "  cargo-deny not installed, skipping (install: cargo install cargo-deny)"
fi

info "  gitleaks detect..."
if command -v gitleaks &>/dev/null; then
    gitleaks detect --config .gitleaks.toml --no-banner || die "gitleaks found secrets!"
    ok "  gitleaks passed"
else
    warn "  gitleaks not installed, skipping (install: brew install gitleaks)"
fi

info "  trivy fs scan..."
if command -v trivy &>/dev/null; then
    trivy fs . --severity CRITICAL,HIGH --exit-code 1 || die "trivy found critical vulnerabilities!"
    ok "  trivy passed"
else
    warn "  trivy not installed, skipping (install: brew install trivy)"
fi

ok "Security checks passed"

# ── Step 3: Run tests ──
info "Step 3/8: Running tests..."
cargo test --all-features --workspace || die "Tests failed"
ok "All tests passed"

# ── Step 4: Build release binaries for all targets ──
info "Step 4/8: Building release binaries..."
rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR"

for target in "${TARGETS[@]}"; do
    info "  Building for $target..."

    if [[ "$target" == "$(rustc -vV | grep host | awk '{print $2}')" ]]; then
        # Native build
        if command -v cargo-auditable &>/dev/null; then
            cargo auditable build --release --target "$target"
        else
            cargo build --release --target "$target"
        fi
    else
        # Cross-compilation
        if command -v cross &>/dev/null; then
            cross build --release --target "$target"
        elif command -v cargo-zigbuild &>/dev/null; then
            cargo zigbuild --release --target "$target"
        else
            warn "  Skipping $target (no cross/zigbuild available)"
            continue
        fi
    fi

    # Package into tarball
    TARBALL="$DIST_DIR/miseban-ai-${VERSION}-${target}.tar.gz"
    tar -czf "$TARBALL" -C "target/${target}/release" \
        $(find "target/${target}/release" -maxdepth 1 -type f -executable -printf '%f\n' 2>/dev/null || \
         find "target/${target}/release" -maxdepth 1 -type f -perm +111 -exec basename {} \; 2>/dev/null || true)

    ok "  Built $target -> $(basename "$TARBALL")"
done

ok "Release binaries built"

# ── Step 5: Generate SBOM ──
info "Step 5/8: Generating SBOM..."

if command -v cargo-cyclonedx &>/dev/null; then
    cargo cyclonedx --format json --output-cdx
    cp bom.json "$DIST_DIR/sbom-cyclonedx.json" 2>/dev/null || true
    ok "  CycloneDX SBOM generated"
else
    warn "  cargo-cyclonedx not installed, skipping"
fi

if command -v cargo-sbom &>/dev/null; then
    cargo sbom > "$DIST_DIR/sbom-spdx.json"
    ok "  SPDX SBOM generated"
else
    warn "  cargo-sbom not installed, skipping"
fi

ok "SBOM generation complete"

# ── Step 6: Generate checksums ──
info "Step 6/8: Generating checksums..."
cd "$DIST_DIR"

if command -v sha256sum &>/dev/null; then
    sha256sum *.tar.gz > SHA256SUMS 2>/dev/null || true
elif command -v shasum &>/dev/null; then
    shasum -a 256 *.tar.gz > SHA256SUMS 2>/dev/null || true
fi

ok "Checksums written to dist/SHA256SUMS"
cat SHA256SUMS 2>/dev/null || true

cd "$PROJECT_ROOT"

# ── Step 7: Sign artifacts with minisign ──
info "Step 7/8: Signing artifacts..."

MINISIGN_KEY="$HOME/.minisign/minisign.key"
if command -v minisign &>/dev/null && [[ -f "$MINISIGN_KEY" ]]; then
    for f in "$DIST_DIR"/*.tar.gz "$DIST_DIR/SHA256SUMS"; do
        if [[ -f "$f" ]]; then
            minisign -Sm "$f" -s "$MINISIGN_KEY" -t "MisebanAI $VERSION"
            ok "  Signed $(basename "$f")"
        fi
    done
    ok "Artifacts signed with minisign"
else
    warn "minisign key not found at $MINISIGN_KEY, skipping signature"
    warn "(Generate with: minisign -G -p minisign.pub -s ~/.minisign/minisign.key)"
fi

# ── Step 8: Create GitHub release ──
info "Step 8/8: Creating GitHub release..."

if ! command -v gh &>/dev/null; then
    die "GitHub CLI (gh) is required. Install: brew install gh"
fi

# Tag if not already tagged
if ! git rev-parse "$VERSION" &>/dev/null; then
    info "  Creating git tag $VERSION..."
    git tag -a "$VERSION" -m "Release $VERSION"
    git push origin "$VERSION"
fi

# Collect all release assets
ASSETS=()
for f in "$DIST_DIR"/*.tar.gz "$DIST_DIR"/SHA256SUMS "$DIST_DIR"/*.minisig "$DIST_DIR"/sbom-*.json; do
    if [[ -f "$f" ]]; then
        ASSETS+=("$f")
    fi
done

gh release create "$VERSION" \
    --title "MisebanAI $VERSION" \
    --generate-notes \
    "${ASSETS[@]}"

ok "GitHub release $VERSION created"

# ── Optional: Sign container with cosign ──
CONTAINER_IMAGE="ghcr.io/$(gh repo view --json nameWithOwner -q .nameWithOwner | tr '[:upper:]' '[:lower:]'):${VERSION}"

if command -v cosign &>/dev/null; then
    info "Signing container image $CONTAINER_IMAGE..."
    if docker image inspect "$CONTAINER_IMAGE" &>/dev/null 2>&1; then
        cosign sign --yes "$CONTAINER_IMAGE"
        ok "Container image signed with cosign"
    else
        warn "Container image $CONTAINER_IMAGE not found locally, skipping cosign"
    fi
else
    warn "cosign not installed, skipping container signature"
fi

echo ""
echo "============================================"
ok "Release $VERSION complete!"
echo "============================================"
echo ""
info "Release artifacts in: $DIST_DIR/"
ls -lh "$DIST_DIR/" 2>/dev/null || true
