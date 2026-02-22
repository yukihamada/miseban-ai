# ================================================================
# MisebanAI — Development & Security Makefile
# ================================================================

.DEFAULT_GOAL := help
SHELL := /bin/bash
.ONESHELL:

VERSION     ?= $(shell git describe --tags --always --dirty 2>/dev/null || echo "dev")
REGISTRY    ?= ghcr.io
IMAGE_NAME  ?= $(shell basename $(CURDIR))
IMAGE_TAG   ?= $(VERSION)
IMAGE       := $(REGISTRY)/$(IMAGE_NAME):$(IMAGE_TAG)

# ────────────────────────────────────────────
# Development
# ────────────────────────────────────────────

.PHONY: check
check: ## Run fmt check, clippy, and tests
	cargo fmt --all -- --check
	cargo clippy --all-targets --all-features -- -D warnings
	cargo test --all-features --workspace

.PHONY: build
build: ## Build release binary with auditable provenance
	cargo auditable build --release

.PHONY: run-api
run-api: ## Run the API server
	cargo run -p api

.PHONY: run-agent
run-agent: ## Run the agent with example config
	cargo run -p agent -- --config config.example.toml

.PHONY: fmt
fmt: ## Auto-format code
	cargo fmt --all

.PHONY: test
test: ## Run all tests
	cargo test --all-features --workspace

.PHONY: clippy
clippy: ## Run clippy linter
	cargo clippy --all-targets --all-features -- -D warnings

# ────────────────────────────────────────────
# Security
# ────────────────────────────────────────────

.PHONY: security
security: audit secrets scan ## Run all security checks (deny + gitleaks + trivy)
	@echo "=== All security checks passed ==="

.PHONY: audit
audit: ## Run cargo-deny checks (advisories, licenses, bans, sources)
	cargo deny check

.PHONY: secrets
secrets: ## Detect secrets with gitleaks
	gitleaks detect --config .gitleaks.toml --verbose

.PHONY: scan
scan: ## Filesystem vulnerability scan with Trivy
	trivy fs . --severity CRITICAL,HIGH

.PHONY: sbom
sbom: ## Generate Software Bill of Materials
	@echo "=== Generating CycloneDX SBOM ==="
	cargo install cargo-cyclonedx 2>/dev/null || true
	cargo cyclonedx --format json --output-cdx
	@echo "=== Generating SPDX SBOM ==="
	cargo install cargo-sbom 2>/dev/null || true
	cargo sbom > sbom.spdx.json
	@echo "=== SBOMs generated ==="

# ────────────────────────────────────────────
# Docker
# ────────────────────────────────────────────

.PHONY: docker
docker: ## Build Docker images
	docker build -f Dockerfile.web -t $(IMAGE) .
	docker tag $(IMAGE) $(REGISTRY)/$(IMAGE_NAME):latest

.PHONY: docker-scan
docker-scan: docker ## Scan Docker image with Trivy
	trivy image $(IMAGE) --severity CRITICAL,HIGH

.PHONY: docker-sign
docker-sign: ## Sign Docker image with cosign (requires cosign + OIDC or key)
	cosign sign --yes $(IMAGE)

# ────────────────────────────────────────────
# Release
# ────────────────────────────────────────────

.PHONY: release
release: ## Full release pipeline (VERSION=v1.0.0)
ifndef VERSION
	$(error VERSION is not set. Usage: make release VERSION=v1.0.0)
endif
	@echo "=== Starting release $(VERSION) ==="
	./scripts/release.sh $(VERSION)

# ────────────────────────────────────────────
# Deploy
# ────────────────────────────────────────────

.PHONY: deploy
deploy: ## Deploy to Fly.io
	fly deploy -c fly.web.toml --remote-only

# ────────────────────────────────────────────
# Shipping (physical product)
# ────────────────────────────────────────────

.PHONY: ship-label
ship-label: ## Generate shipping label via Ship&co API (ORDER_ID=xxx)
ifndef ORDER_ID
	$(error ORDER_ID is not set. Usage: make ship-label ORDER_ID=xxx)
endif
	@echo "=== Generating shipping label for order $(ORDER_ID) ==="
	@if [ -z "$${SHIPANDCO_API_KEY}" ]; then \
		echo "Error: SHIPANDCO_API_KEY environment variable is not set"; \
		exit 1; \
	fi
	curl -s -X POST "https://app.shipandco.com/api/v1/labels" \
		-H "Authorization: Bearer $${SHIPANDCO_API_KEY}" \
		-H "Content-Type: application/json" \
		-d '{"order_id": "$(ORDER_ID)"}' \
		| jq .

# ────────────────────────────────────────────
# Utilities
# ────────────────────────────────────────────

.PHONY: clean
clean: ## Clean build artifacts
	cargo clean
	rm -rf dist/ sbom.spdx.json

.PHONY: install-tools
install-tools: ## Install all required security/build tools
	cargo install cargo-deny cargo-auditable cargo-cyclonedx cargo-sbom --locked
	@echo "Also install (via your package manager):"
	@echo "  - gitleaks   : https://github.com/gitleaks/gitleaks"
	@echo "  - trivy      : https://github.com/aquasecurity/trivy"
	@echo "  - cosign     : https://github.com/sigstore/cosign"
	@echo "  - minisign   : https://github.com/jedisct1/minisign"
	@echo "  - syft       : https://github.com/anchore/syft"

.PHONY: help
help: ## Show this help
	@printf "\033[1mMisebanAI Makefile\033[0m\n\n"
	@printf "\033[36m%-20s\033[0m %s\n" "Target" "Description"
	@printf "%-20s %s\n" "------" "-----------"
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-20s\033[0m %s\n", $$1, $$2}'
