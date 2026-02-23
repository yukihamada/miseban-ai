-- =============================================================================
-- MisebanAI Database Schema
-- =============================================================================
-- Runnable directly in Supabase SQL Editor.
-- Provides: stores, cameras, visitor_counts (time-series), daily_reports,
--           alerts, api_tokens, RLS policies, helper functions, and triggers.
-- =============================================================================

-- ---------------------------------------------------------------------------
-- 0. Extensions
-- ---------------------------------------------------------------------------
CREATE EXTENSION IF NOT EXISTS "pgcrypto";   -- gen_random_uuid()

-- ---------------------------------------------------------------------------
-- 1. Tables
-- ---------------------------------------------------------------------------

-- 1-1. stores -- Shop / store master
-- Cycle 4: Added stripe_customer_id, line_user_id columns for billing & LINE integration.
CREATE TABLE stores (
    id                  uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id            uuid        NOT NULL REFERENCES auth.users (id) ON DELETE CASCADE,
    name                text        NOT NULL,
    address             text,
    plan_tier           text        NOT NULL DEFAULT 'free'
                                    CHECK (plan_tier IN ('free', 'starter', 'pro', 'enterprise')),
    stripe_customer_id  text        UNIQUE,
    line_user_id        text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

COMMENT ON TABLE  stores IS 'Shop / store master record. One owner can have many stores.';
COMMENT ON COLUMN stores.plan_tier IS 'Subscription tier: free | starter | pro | enterprise';

-- 1-2. cameras -- Camera devices registered to a store
CREATE TABLE cameras (
    id           uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    store_id     uuid        NOT NULL REFERENCES stores (id) ON DELETE CASCADE,
    name         text        NOT NULL,
    rtsp_url     text,                          -- encrypted at the application layer
    status       text        NOT NULL DEFAULT 'offline'
                             CHECK (status IN ('online', 'offline', 'error')),
    last_seen_at timestamptz,
    config_json  jsonb       NOT NULL DEFAULT '{}'::jsonb,
    created_at   timestamptz NOT NULL DEFAULT now()
);

COMMENT ON TABLE  cameras IS 'Camera devices per store. rtsp_url should be encrypted in practice.';
COMMENT ON COLUMN cameras.config_json IS 'Arbitrary per-camera configuration (detection zones, thresholds, etc.)';

-- 1-3. visitor_counts -- Per-frame / per-interval visitor counts (time-series)
CREATE TABLE visitor_counts (
    id               bigserial   PRIMARY KEY,
    camera_id        uuid        NOT NULL REFERENCES cameras (id) ON DELETE CASCADE,
    store_id         uuid        NOT NULL REFERENCES stores  (id) ON DELETE CASCADE,  -- denormalized
    counted_at       timestamptz NOT NULL,
    people_count     integer     NOT NULL DEFAULT 0,
    demographics_json jsonb,                    -- e.g. {"male_20s": 3, "female_30s": 1}
    zones_json       jsonb                      -- zone-level heatmap data
);

COMMENT ON TABLE  visitor_counts IS 'High-frequency visitor count records. Denormalized store_id for fast queries.';
COMMENT ON COLUMN visitor_counts.demographics_json IS 'Optional age/gender breakdown for this frame.';
COMMENT ON COLUMN visitor_counts.zones_json IS 'Optional zone-level heatmap / occupancy data.';

-- Indexes for time-series queries
CREATE INDEX idx_visitor_counts_store_time  ON visitor_counts (store_id,  counted_at DESC);
CREATE INDEX idx_visitor_counts_camera_time ON visitor_counts (camera_id, counted_at DESC);

-- 1-4. daily_reports -- Aggregated daily statistics
CREATE TABLE daily_reports (
    id                   uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    store_id             uuid        NOT NULL REFERENCES stores (id) ON DELETE CASCADE,
    report_date          date        NOT NULL,
    total_visitors       bigint      NOT NULL DEFAULT 0,
    peak_hour            smallint,                       -- 0-23
    hourly_counts        jsonb,                          -- {"0": 5, "1": 2, ..., "23": 8}
    demographics_summary jsonb,                          -- aggregated age/gender
    created_at           timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT uq_daily_report_store_date UNIQUE (store_id, report_date)
);

COMMENT ON TABLE  daily_reports IS 'One row per store per day. Aggregated from visitor_counts.';
COMMENT ON COLUMN daily_reports.peak_hour IS 'Hour (0-23) with the highest visitor count.';

-- 1-5. alerts -- Alert / anomaly events
CREATE TABLE alerts (
    id          uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    store_id    uuid        NOT NULL REFERENCES stores  (id) ON DELETE CASCADE,
    camera_id   uuid        REFERENCES cameras (id) ON DELETE SET NULL,
    alert_type  text        NOT NULL
                            CHECK (alert_type IN ('intrusion', 'unusual', 'crowding')),
    confidence  real,                            -- 0.0 - 1.0
    message     text,
    is_read     boolean     NOT NULL DEFAULT false,
    created_at  timestamptz NOT NULL DEFAULT now()
);

COMMENT ON TABLE alerts IS 'Alert events detected by the vision pipeline.';

CREATE INDEX idx_alerts_store_created ON alerts (store_id, created_at DESC);

-- 1-6. api_tokens -- API tokens for camera agent authentication
CREATE TABLE api_tokens (
    id           uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    store_id     uuid        NOT NULL REFERENCES stores (id) ON DELETE CASCADE,
    token_hash   text        NOT NULL UNIQUE,    -- bcrypt hash of the raw token
    name         text,                            -- human-readable label
    last_used_at timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz                      -- NULL = never expires
);

COMMENT ON TABLE  api_tokens IS 'Hashed API tokens for headless camera agents to push data.';
COMMENT ON COLUMN api_tokens.token_hash IS 'bcrypt hash. Raw token is shown once at creation and never stored.';

-- ---------------------------------------------------------------------------
-- 2. Row Level Security (RLS)
-- ---------------------------------------------------------------------------

ALTER TABLE stores          ENABLE ROW LEVEL SECURITY;
ALTER TABLE cameras         ENABLE ROW LEVEL SECURITY;
ALTER TABLE visitor_counts  ENABLE ROW LEVEL SECURITY;
ALTER TABLE daily_reports   ENABLE ROW LEVEL SECURITY;
ALTER TABLE alerts          ENABLE ROW LEVEL SECURITY;
ALTER TABLE api_tokens      ENABLE ROW LEVEL SECURITY;

-- Helper: check if the current user owns a given store
CREATE OR REPLACE FUNCTION is_store_owner(p_store_id uuid)
RETURNS boolean
LANGUAGE sql
STABLE
SECURITY DEFINER
AS $$
    SELECT EXISTS (
        SELECT 1 FROM stores WHERE id = p_store_id AND owner_id = auth.uid()
    );
$$;

-- -- stores ------------------------------------------------------------------
CREATE POLICY stores_select ON stores FOR SELECT
    USING (owner_id = auth.uid());

CREATE POLICY stores_insert ON stores FOR INSERT
    WITH CHECK (owner_id = auth.uid());

CREATE POLICY stores_update ON stores FOR UPDATE
    USING (owner_id = auth.uid())
    WITH CHECK (owner_id = auth.uid());

CREATE POLICY stores_delete ON stores FOR DELETE
    USING (owner_id = auth.uid());

-- -- cameras -----------------------------------------------------------------
CREATE POLICY cameras_select ON cameras FOR SELECT
    USING (is_store_owner(store_id));

CREATE POLICY cameras_insert ON cameras FOR INSERT
    WITH CHECK (is_store_owner(store_id));

CREATE POLICY cameras_update ON cameras FOR UPDATE
    USING (is_store_owner(store_id))
    WITH CHECK (is_store_owner(store_id));

CREATE POLICY cameras_delete ON cameras FOR DELETE
    USING (is_store_owner(store_id));

-- -- visitor_counts ----------------------------------------------------------
CREATE POLICY visitor_counts_select ON visitor_counts FOR SELECT
    USING (is_store_owner(store_id));

-- Insert is expected from service_role (camera agents), not end-users.
-- If agents authenticate via api_tokens, grant insert through a server-side function.
CREATE POLICY visitor_counts_insert ON visitor_counts FOR INSERT
    WITH CHECK (is_store_owner(store_id));

-- -- daily_reports -----------------------------------------------------------
CREATE POLICY daily_reports_select ON daily_reports FOR SELECT
    USING (is_store_owner(store_id));

CREATE POLICY daily_reports_insert ON daily_reports FOR INSERT
    WITH CHECK (is_store_owner(store_id));

CREATE POLICY daily_reports_update ON daily_reports FOR UPDATE
    USING (is_store_owner(store_id))
    WITH CHECK (is_store_owner(store_id));

-- -- alerts ------------------------------------------------------------------
CREATE POLICY alerts_select ON alerts FOR SELECT
    USING (is_store_owner(store_id));

CREATE POLICY alerts_update ON alerts FOR UPDATE
    USING (is_store_owner(store_id))
    WITH CHECK (is_store_owner(store_id));

-- -- api_tokens --------------------------------------------------------------
CREATE POLICY api_tokens_select ON api_tokens FOR SELECT
    USING (is_store_owner(store_id));

CREATE POLICY api_tokens_insert ON api_tokens FOR INSERT
    WITH CHECK (is_store_owner(store_id));

CREATE POLICY api_tokens_delete ON api_tokens FOR DELETE
    USING (is_store_owner(store_id));

-- ---------------------------------------------------------------------------
-- 3. Triggers
-- ---------------------------------------------------------------------------

-- 3-1. Auto-update `updated_at` on stores
CREATE OR REPLACE FUNCTION trigger_set_updated_at()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    NEW.updated_at = now();
    RETURN NEW;
END;
$$;

CREATE TRIGGER trg_stores_updated_at
    BEFORE UPDATE ON stores
    FOR EACH ROW
    EXECUTE FUNCTION trigger_set_updated_at();

-- 3-2. Auto-populate store_id from camera on visitor_counts INSERT
--      Allows agents to omit store_id if they provide camera_id.
CREATE OR REPLACE FUNCTION trigger_set_store_id_from_camera()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.store_id IS NULL THEN
        SELECT store_id INTO NEW.store_id
        FROM cameras
        WHERE id = NEW.camera_id;

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

-- 4-1. get_hourly_counts
--      Returns 24 rows (hour 0-23) with total people_count for a store on a date.
CREATE OR REPLACE FUNCTION get_hourly_counts(p_store_id uuid, p_date date)
RETURNS TABLE (hour integer, total_count bigint)
LANGUAGE sql
STABLE
SECURITY DEFINER
AS $$
    WITH hours AS (
        SELECT generate_series(0, 23) AS h
    )
    SELECT
        h.h::integer                             AS hour,
        COALESCE(SUM(vc.people_count), 0)::bigint AS total_count
    FROM hours h
    LEFT JOIN visitor_counts vc
        ON  vc.store_id   = p_store_id
        AND vc.counted_at >= (p_date + make_interval(hours => h.h))
        AND vc.counted_at <  (p_date + make_interval(hours => h.h + 1))
    GROUP BY h.h
    ORDER BY h.h;
$$;

COMMENT ON FUNCTION get_hourly_counts IS 'Returns 24 rows with hourly visitor totals for a given store and date.';

-- 4-2. get_weekly_summary
--      Returns the last 7 days of daily totals (from daily_reports).
CREATE OR REPLACE FUNCTION get_weekly_summary(p_store_id uuid)
RETURNS TABLE (report_date date, total_visitors bigint, peak_hour smallint)
LANGUAGE sql
STABLE
SECURITY DEFINER
AS $$
    SELECT
        dr.report_date,
        dr.total_visitors,
        dr.peak_hour
    FROM daily_reports dr
    WHERE dr.store_id    = p_store_id
      AND dr.report_date >= CURRENT_DATE - INTERVAL '6 days'
    ORDER BY dr.report_date;
$$;

COMMENT ON FUNCTION get_weekly_summary IS 'Returns the last 7 days of daily visitor totals for a store.';

-- 4-3. aggregate_daily_report
--      Creates or updates a daily_report row by aggregating visitor_counts.
CREATE OR REPLACE FUNCTION aggregate_daily_report(p_store_id uuid, p_date date)
RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
AS $$
DECLARE
    v_total        bigint;
    v_peak_hour    smallint;
    v_hourly       jsonb;
    v_demographics jsonb;
BEGIN
    -- Calculate hourly counts
    SELECT
        jsonb_object_agg(h.hour::text, h.cnt),
        SUM(h.cnt),
        (ARRAY_AGG(h.hour ORDER BY h.cnt DESC))[1]
    INTO v_hourly, v_total, v_peak_hour
    FROM (
        SELECT
            EXTRACT(HOUR FROM vc.counted_at)::integer AS hour,
            SUM(vc.people_count)::bigint               AS cnt
        FROM visitor_counts vc
        WHERE vc.store_id   = p_store_id
          AND vc.counted_at >= p_date::timestamptz
          AND vc.counted_at <  (p_date + INTERVAL '1 day')::timestamptz
        GROUP BY EXTRACT(HOUR FROM vc.counted_at)
    ) h;

    -- If no data, set sensible defaults
    IF v_total IS NULL THEN
        v_total     := 0;
        v_peak_hour := NULL;
        v_hourly    := '{}'::jsonb;
    END IF;

    -- Aggregate demographics (merge all JSON keys by summing values)
    SELECT jsonb_object_agg(kv.key, kv.val)
    INTO v_demographics
    FROM (
        SELECT
            d.key,
            SUM(d.value::text::bigint)::bigint AS val
        FROM visitor_counts vc,
             jsonb_each(vc.demographics_json) AS d(key, value)
        WHERE vc.store_id   = p_store_id
          AND vc.counted_at >= p_date::timestamptz
          AND vc.counted_at <  (p_date + INTERVAL '1 day')::timestamptz
          AND vc.demographics_json IS NOT NULL
        GROUP BY d.key
    ) kv;

    -- Upsert daily_report
    INSERT INTO daily_reports (store_id, report_date, total_visitors, peak_hour, hourly_counts, demographics_summary)
    VALUES (p_store_id, p_date, v_total, v_peak_hour, v_hourly, v_demographics)
    ON CONFLICT (store_id, report_date)
    DO UPDATE SET
        total_visitors       = EXCLUDED.total_visitors,
        peak_hour            = EXCLUDED.peak_hour,
        hourly_counts        = EXCLUDED.hourly_counts,
        demographics_summary = EXCLUDED.demographics_summary,
        created_at           = now();
END;
$$;

COMMENT ON FUNCTION aggregate_daily_report IS 'Aggregates visitor_counts into a daily_report row (upsert).';

-- ---------------------------------------------------------------------------
-- 5. Seed Data (for development only -- uncomment to use)
-- ---------------------------------------------------------------------------

/*
-- NOTE: Replace '00000000-0000-0000-0000-000000000001' with a real auth.users id
--       when running in a Supabase project.
-- For local development, create a user first:
--   INSERT INTO auth.users (id, email) VALUES ('00000000-0000-0000-0000-000000000001', 'demo@miseban.ai');

DO $$
DECLARE
    v_owner_id  uuid := '00000000-0000-0000-0000-000000000001';
    v_store_id  uuid;
    v_cam_ids   uuid[];
    v_day       date;
    v_hour      integer;
    v_count     integer;
    v_hourly    jsonb;
    v_total     bigint;
    v_peak      smallint;
    v_peak_cnt  bigint;
BEGIN
    -- 5-1. Demo store
    INSERT INTO stores (owner_id, name, address, plan_tier)
    VALUES (v_owner_id, 'Demo Cafe Shibuya', '東京都渋谷区道玄坂1-2-3', 'pro')
    RETURNING id INTO v_store_id;

    -- 5-2. Three cameras
    INSERT INTO cameras (store_id, name, status) VALUES
        (v_store_id, 'Entrance Camera',     'online'),
        (v_store_id, 'Counter Camera',      'online'),
        (v_store_id, 'Terrace Camera',      'offline')
    RETURNING ARRAY_AGG(id) INTO v_cam_ids;

    -- 5-3. 7 days of visitor_counts (realistic cafe pattern)
    --      Peak hours: 8-9 (morning), 12-13 (lunch), 18-19 (evening)
    FOR d IN 0..6 LOOP
        v_day := CURRENT_DATE - (6 - d);  -- oldest first

        FOR v_hour IN 0..23 LOOP
            -- Simulate a realistic hourly pattern
            v_count := CASE
                WHEN v_hour BETWEEN 0  AND 5  THEN floor(random() * 3)::integer              -- late night: 0-2
                WHEN v_hour BETWEEN 6  AND 7  THEN floor(random() * 8  + 3)::integer          -- early morning: 3-10
                WHEN v_hour BETWEEN 8  AND 9  THEN floor(random() * 15 + 20)::integer         -- morning rush: 20-34
                WHEN v_hour BETWEEN 10 AND 11 THEN floor(random() * 10 + 10)::integer         -- mid-morning: 10-19
                WHEN v_hour BETWEEN 12 AND 13 THEN floor(random() * 20 + 25)::integer         -- lunch rush: 25-44
                WHEN v_hour BETWEEN 14 AND 16 THEN floor(random() * 10 + 8)::integer          -- afternoon: 8-17
                WHEN v_hour BETWEEN 17 AND 19 THEN floor(random() * 15 + 15)::integer         -- evening: 15-29
                WHEN v_hour BETWEEN 20 AND 21 THEN floor(random() * 10 + 5)::integer          -- late evening: 5-14
                ELSE floor(random() * 5)::integer                                              -- closing: 0-4
            END;

            -- Insert one record per hour from the entrance camera
            INSERT INTO visitor_counts (camera_id, store_id, counted_at, people_count, demographics_json)
            VALUES (
                v_cam_ids[1],
                v_store_id,
                v_day + make_interval(hours => v_hour, mins => 30),  -- :30 of each hour
                v_count,
                jsonb_build_object(
                    'male_20s',   floor(v_count * 0.3)::integer,
                    'female_20s', floor(v_count * 0.25)::integer,
                    'male_30s',   floor(v_count * 0.2)::integer,
                    'female_30s', floor(v_count * 0.15)::integer,
                    'other',      v_count - floor(v_count * 0.3)::integer
                                        - floor(v_count * 0.25)::integer
                                        - floor(v_count * 0.2)::integer
                                        - floor(v_count * 0.15)::integer
                )
            );
        END LOOP;
    END LOOP;

    -- 5-4. Generate daily_reports from the seed visitor_counts
    FOR d IN 0..6 LOOP
        v_day := CURRENT_DATE - (6 - d);
        PERFORM aggregate_daily_report(v_store_id, v_day);
    END LOOP;

    RAISE NOTICE 'Seed data created: store=%, cameras=%', v_store_id, v_cam_ids;
END;
$$;
*/

-- ---------------------------------------------------------------------------
-- Data retention policy: delete old visitor_counts based on plan tier
-- ---------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION cleanup_expired_visitor_counts()
RETURNS integer
LANGUAGE plpgsql
SECURITY DEFINER
AS $$
DECLARE
    deleted_count integer := 0;
    store RECORD;
    retention_days integer;
    cutoff timestamp with time zone;
BEGIN
    FOR store IN SELECT id, plan_tier FROM stores LOOP
        -- Determine retention days based on plan tier
        CASE store.plan_tier
            WHEN 'starter' THEN retention_days := 30;
            WHEN 'pro' THEN retention_days := 90;
            WHEN 'enterprise' THEN retention_days := 3650; -- ~10 years
            ELSE retention_days := 7; -- free
        END CASE;

        cutoff := NOW() - (retention_days || ' days')::interval;

        DELETE FROM visitor_counts
        WHERE store_id = store.id
          AND counted_at < cutoff;

        deleted_count := deleted_count + FOUND::integer;
    END LOOP;

    RETURN deleted_count;
END;
$$;

-- Schedule: call cleanup_expired_visitor_counts() daily via pg_cron or
-- application-level cron (e.g., Supabase Edge Function on schedule).

-- ---------------------------------------------------------------------------
-- Done. Schema is ready.
-- ---------------------------------------------------------------------------
