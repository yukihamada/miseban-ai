-- =============================================================================
-- Pairing codes: ephemeral 6-digit codes for agent-to-store binding
-- =============================================================================

CREATE TABLE IF NOT EXISTS pairing_codes (
    code       text        PRIMARY KEY,
    store_id   uuid        NOT NULL REFERENCES stores (id) ON DELETE CASCADE,
    token      text        NOT NULL,
    expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_pairing_codes_expires ON pairing_codes (expires_at);
