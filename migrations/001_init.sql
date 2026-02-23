-- =============================================================================
-- MisebanAI Database Schema (Fly Postgres compatible)
-- =============================================================================
-- Adapted from supabase/schema.sql. Removes Supabase-specific auth.users
-- references and RLS policies. Access control is handled by the Rust API layer.
-- =============================================================================

-- Extensions
CREATE EXTENSION IF NOT EXISTS "pgcrypto";

-- ---------------------------------------------------------------------------
-- 1. Users table (replaces Supabase auth.users)
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS users (
    id             uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    email          text        NOT NULL UNIQUE,
    password_hash  text        NOT NULL,
    created_at     timestamptz NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- 2. Tables
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS stores (
    id                  uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id            uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name                text        NOT NULL,
    address             text,
    plan_tier           text        NOT NULL DEFAULT 'free'
                                    CHECK (plan_tier IN ('free', 'starter', 'pro', 'enterprise')),
    stripe_customer_id  text        UNIQUE,
    line_user_id        text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS cameras (
    id           uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    store_id     uuid        NOT NULL REFERENCES stores (id) ON DELETE CASCADE,
    name         text        NOT NULL,
    rtsp_url     text,
    status       text        NOT NULL DEFAULT 'offline'
                             CHECK (status IN ('online', 'offline', 'error')),
    last_seen_at timestamptz,
    config_json  jsonb       NOT NULL DEFAULT '{}'::jsonb,
    created_at   timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS visitor_counts (
    id                bigserial   PRIMARY KEY,
    camera_id         uuid        NOT NULL REFERENCES cameras (id) ON DELETE CASCADE,
    store_id          uuid        NOT NULL REFERENCES stores  (id) ON DELETE CASCADE,
    counted_at        timestamptz NOT NULL,
    people_count      integer     NOT NULL DEFAULT 0,
    demographics_json jsonb,
    zones_json        jsonb
);

CREATE INDEX IF NOT EXISTS idx_visitor_counts_store_time  ON visitor_counts (store_id,  counted_at DESC);
CREATE INDEX IF NOT EXISTS idx_visitor_counts_camera_time ON visitor_counts (camera_id, counted_at DESC);

CREATE TABLE IF NOT EXISTS daily_reports (
    id                   uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    store_id             uuid        NOT NULL REFERENCES stores (id) ON DELETE CASCADE,
    report_date          date        NOT NULL,
    total_visitors       bigint      NOT NULL DEFAULT 0,
    peak_hour            smallint,
    hourly_counts        jsonb,
    demographics_summary jsonb,
    created_at           timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT uq_daily_report_store_date UNIQUE (store_id, report_date)
);

CREATE TABLE IF NOT EXISTS alerts (
    id          uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    store_id    uuid        NOT NULL REFERENCES stores  (id) ON DELETE CASCADE,
    camera_id   uuid        REFERENCES cameras (id) ON DELETE SET NULL,
    alert_type  text        NOT NULL
                            CHECK (alert_type IN ('intrusion', 'unusual', 'crowding')),
    confidence  real,
    message     text,
    is_read     boolean     NOT NULL DEFAULT false,
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_alerts_store_created ON alerts (store_id, created_at DESC);

CREATE TABLE IF NOT EXISTS api_tokens (
    id           uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    store_id     uuid        NOT NULL REFERENCES stores (id) ON DELETE CASCADE,
    token_hash   text        NOT NULL UNIQUE,
    name         text,
    last_used_at timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz
);

-- ---------------------------------------------------------------------------
-- 3. Triggers
-- ---------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION trigger_set_updated_at()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.updated_at = now();
    RETURN NEW;
END;
$$;

CREATE TRIGGER trg_stores_updated_at
    BEFORE UPDATE ON stores
    FOR EACH ROW
    EXECUTE FUNCTION trigger_set_updated_at();

CREATE OR REPLACE FUNCTION trigger_set_store_id_from_camera()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.store_id IS NULL THEN
        SELECT store_id INTO NEW.store_id
        FROM cameras WHERE id = NEW.camera_id;
        IF NEW.store_id IS NULL THEN
            RAISE EXCEPTION 'Camera % not found', NEW.camera_id;
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER trg_visitor_counts_set_store_id
    BEFORE INSERT ON visitor_counts
    FOR EACH ROW
    EXECUTE FUNCTION trigger_set_store_id_from_camera();

-- ---------------------------------------------------------------------------
-- 4. Functions
-- ---------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION get_hourly_counts(p_store_id uuid, p_date date)
RETURNS TABLE (hour integer, total_count bigint) LANGUAGE sql STABLE AS $$
    WITH hours AS (SELECT generate_series(0, 23) AS h)
    SELECT
        h.h::integer AS hour,
        COALESCE(SUM(vc.people_count), 0)::bigint AS total_count
    FROM hours h
    LEFT JOIN visitor_counts vc
        ON  vc.store_id   = p_store_id
        AND vc.counted_at >= (p_date + make_interval(hours => h.h))
        AND vc.counted_at <  (p_date + make_interval(hours => h.h + 1))
    GROUP BY h.h
    ORDER BY h.h;
$$;

CREATE OR REPLACE FUNCTION get_weekly_summary(p_store_id uuid)
RETURNS TABLE (report_date date, total_visitors bigint, peak_hour smallint) LANGUAGE sql STABLE AS $$
    SELECT dr.report_date, dr.total_visitors, dr.peak_hour
    FROM daily_reports dr
    WHERE dr.store_id = p_store_id
      AND dr.report_date >= CURRENT_DATE - INTERVAL '6 days'
    ORDER BY dr.report_date;
$$;

-- Data retention cleanup
CREATE OR REPLACE FUNCTION cleanup_expired_visitor_counts()
RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM visitor_counts
    WHERE id IN (
        SELECT vc.id FROM visitor_counts vc
        JOIN stores s ON s.id = vc.store_id
        WHERE (s.plan_tier = 'free'       AND vc.counted_at < NOW() - INTERVAL '7 days')
           OR (s.plan_tier = 'starter'    AND vc.counted_at < NOW() - INTERVAL '30 days')
           OR (s.plan_tier = 'pro'        AND vc.counted_at < NOW() - INTERVAL '90 days')
           OR (s.plan_tier = 'enterprise' AND vc.counted_at < NOW() - INTERVAL '365 days')
    );
END;
$$;
