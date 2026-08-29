-- Add native block coordinates where a complete BlockRef is persisted.
--
-- Before applying this migration to populated state, the migration session
-- must set payment_sdk.verified_dense_scopes to a JSON array of exact scopes
-- whose chain source has been verified as dense. Only Bitcoin and Ethereum
-- scopes are eligible, for example:
--
--   SELECT set_config(
--       'payment_sdk.verified_dense_scopes',
--       '[{"chain":"bitcoin","network":"regtest"}]',
--       false
--   );
--
-- An empty database needs no allowlist. Any populated scope not present in the
-- allowlist aborts the complete transaction. The final constraints are added
-- and validated only after the complete backfill validation succeeds.

BEGIN;

ALTER TABLE checkpoint ADD COLUMN position bigint;
ALTER TABLE checkpoint ADD COLUMN parent_position bigint;

ALTER TABLE history ADD COLUMN block_position bigint;
ALTER TABLE history ADD COLUMN block_parent_position bigint;

ALTER TABLE journal ADD COLUMN block_position bigint;
ALTER TABLE journal ADD COLUMN block_parent_position bigint;
ALTER TABLE journal ADD COLUMN previous_checkpoint_position bigint;
ALTER TABLE journal ADD COLUMN previous_checkpoint_parent_position bigint;

DO $backfill_guard$
DECLARE
    configured jsonb := COALESCE(
        NULLIF(current_setting('payment_sdk.verified_dense_scopes', true), ''),
        '[]'
    )::jsonb;
BEGIN
    IF jsonb_typeof(configured) IS DISTINCT FROM 'array' THEN
        RAISE EXCEPTION
            'payment_sdk.verified_dense_scopes must be a JSON array';
    END IF;

    IF EXISTS (
        SELECT 1
        FROM jsonb_to_recordset(configured) AS scope(chain text, network text)
        WHERE chain IS NULL
           OR network IS NULL
           OR chain NOT IN ('bitcoin', 'ethereum')
    ) THEN
        RAISE EXCEPTION
            'coordinate backfill allowlist contains an ineligible scope';
    END IF;

    IF EXISTS (
        WITH allowed AS (
            SELECT DISTINCT chain, network
            FROM jsonb_to_recordset(configured)
                AS scope(chain text, network text)
        ),
        populated AS (
            SELECT chain, network FROM checkpoint
            UNION
            SELECT chain, network FROM history
            UNION
            SELECT chain, network FROM movement
            UNION
            SELECT chain, network FROM output
            UNION
            SELECT chain, network FROM journal
            UNION
            SELECT chain, network FROM journal_output
        )
        SELECT 1
        FROM populated
        LEFT JOIN allowed USING (chain, network)
        WHERE allowed.chain IS NULL
    ) THEN
        RAISE EXCEPTION
            'coordinate backfill found an unverified populated scope';
    END IF;

    IF EXISTS (
        SELECT 1
        FROM checkpoint
        WHERE height < 0
           OR ((height = 0) <> (parent_hash IS NULL))
    ) OR EXISTS (
        SELECT 1
        FROM history
        WHERE height < 0
           OR ((height = 0) <> (block_parent IS NULL))
    ) OR EXISTS (
        SELECT 1
        FROM journal
        WHERE height < 0
           OR ((height = 0) <> (block_parent IS NULL))
           OR ((previous_checkpoint_height IS NULL)
               <> (previous_checkpoint_hash IS NULL))
           OR (previous_checkpoint_height IS NULL
               AND previous_checkpoint_time IS NOT NULL)
           OR (previous_checkpoint_height IS NOT NULL
               AND previous_checkpoint_height < 0)
           OR (previous_checkpoint_height = 0
               AND previous_checkpoint_parent IS NOT NULL)
           OR (previous_checkpoint_height > 0
               AND previous_checkpoint_parent IS NULL)
    ) THEN
        RAISE EXCEPTION
            'coordinate backfill found an invalid dense parent relationship';
    END IF;
END
$backfill_guard$;

WITH allowed AS (
    SELECT DISTINCT chain, network
    FROM jsonb_to_recordset(
        COALESCE(
            NULLIF(current_setting('payment_sdk.verified_dense_scopes', true), ''),
            '[]'
        )::jsonb
    ) AS scope(chain text, network text)
)
UPDATE checkpoint AS target
SET position = target.height,
    parent_position = CASE
        WHEN target.parent_hash IS NULL THEN NULL
        ELSE target.height - 1
    END
FROM allowed
WHERE target.chain = allowed.chain
  AND target.network = allowed.network;

WITH allowed AS (
    SELECT DISTINCT chain, network
    FROM jsonb_to_recordset(
        COALESCE(
            NULLIF(current_setting('payment_sdk.verified_dense_scopes', true), ''),
            '[]'
        )::jsonb
    ) AS scope(chain text, network text)
)
UPDATE history AS target
SET block_position = target.height,
    block_parent_position = CASE
        WHEN target.block_parent IS NULL THEN NULL
        ELSE target.height - 1
    END
FROM allowed
WHERE target.chain = allowed.chain
  AND target.network = allowed.network;

WITH allowed AS (
    SELECT DISTINCT chain, network
    FROM jsonb_to_recordset(
        COALESCE(
            NULLIF(current_setting('payment_sdk.verified_dense_scopes', true), ''),
            '[]'
        )::jsonb
    ) AS scope(chain text, network text)
)
UPDATE journal AS target
SET block_position = target.height,
    block_parent_position = CASE
        WHEN target.block_parent IS NULL THEN NULL
        ELSE target.height - 1
    END,
    previous_checkpoint_position = target.previous_checkpoint_height,
    previous_checkpoint_parent_position = CASE
        WHEN target.previous_checkpoint_parent IS NULL THEN NULL
        ELSE target.previous_checkpoint_height - 1
    END
FROM allowed
WHERE target.chain = allowed.chain
  AND target.network = allowed.network;

DO $backfill_validation$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM checkpoint
        WHERE position IS DISTINCT FROM height
           OR (parent_hash IS NULL) <> (parent_position IS NULL)
           OR (parent_position IS NOT NULL
               AND parent_position IS DISTINCT FROM height - 1)
    ) OR EXISTS (
        SELECT 1
        FROM history
        WHERE block_position IS DISTINCT FROM height
           OR (block_parent IS NULL) <> (block_parent_position IS NULL)
           OR (block_parent_position IS NOT NULL
               AND block_parent_position IS DISTINCT FROM height - 1)
    ) OR EXISTS (
        SELECT 1
        FROM journal
        WHERE block_position IS DISTINCT FROM height
           OR (block_parent IS NULL) <> (block_parent_position IS NULL)
           OR (block_parent_position IS NOT NULL
               AND block_parent_position IS DISTINCT FROM height - 1)
           OR previous_checkpoint_position
               IS DISTINCT FROM previous_checkpoint_height
           OR (previous_checkpoint_parent IS NULL)
               <> (previous_checkpoint_parent_position IS NULL)
           OR (previous_checkpoint_parent_position IS NOT NULL
               AND previous_checkpoint_parent_position
                   IS DISTINCT FROM previous_checkpoint_height - 1)
    ) THEN
        RAISE EXCEPTION 'coordinate backfill validation failed';
    END IF;
END
$backfill_validation$;

ALTER TABLE checkpoint
    ADD CONSTRAINT checkpoint_position_nonnegative
        CHECK (position >= 0) NOT VALID,
    ADD CONSTRAINT checkpoint_parent_complete
        CHECK (
            (position = 0
                AND parent_position IS NULL
                AND parent_hash IS NULL)
            OR (position > 0
                AND parent_position IS NOT NULL
                AND parent_hash IS NOT NULL
                AND parent_position >= 0
                AND parent_position < position)
        ) NOT VALID;

ALTER TABLE history
    ADD CONSTRAINT history_block_position_nonnegative
        CHECK (block_position >= 0) NOT VALID,
    ADD CONSTRAINT history_block_parent_complete
        CHECK (
            (block_position = 0
                AND block_parent_position IS NULL
                AND block_parent IS NULL)
            OR (block_position > 0
                AND block_parent_position IS NOT NULL
                AND block_parent IS NOT NULL
                AND block_parent_position >= 0
                AND block_parent_position < block_position)
        ) NOT VALID;

ALTER TABLE journal
    ADD CONSTRAINT journal_block_position_nonnegative
        CHECK (block_position >= 0) NOT VALID,
    ADD CONSTRAINT journal_block_parent_complete
        CHECK (
            (block_position = 0
                AND block_parent_position IS NULL
                AND block_parent IS NULL)
            OR (block_position > 0
                AND block_parent_position IS NOT NULL
                AND block_parent IS NOT NULL
                AND block_parent_position >= 0
                AND block_parent_position < block_position)
        ) NOT VALID,
    ADD CONSTRAINT journal_previous_checkpoint_complete
        CHECK (
            (previous_checkpoint_position IS NULL
                AND previous_checkpoint_height IS NULL
                AND previous_checkpoint_hash IS NULL
                AND previous_checkpoint_parent_position IS NULL
                AND previous_checkpoint_parent IS NULL
                AND previous_checkpoint_time IS NULL)
            OR (previous_checkpoint_position IS NOT NULL
                AND previous_checkpoint_height IS NOT NULL
                AND previous_checkpoint_hash IS NOT NULL
                AND previous_checkpoint_position >= 0
                AND previous_checkpoint_height >= 0
                AND (
                    (previous_checkpoint_position = 0
                        AND previous_checkpoint_parent_position IS NULL
                        AND previous_checkpoint_parent IS NULL)
                    OR (previous_checkpoint_position > 0
                        AND previous_checkpoint_parent_position IS NOT NULL
                        AND previous_checkpoint_parent IS NOT NULL
                        AND previous_checkpoint_parent_position >= 0
                        AND previous_checkpoint_parent_position
                            < previous_checkpoint_position)
                ))
        ) NOT VALID;

ALTER TABLE checkpoint
    VALIDATE CONSTRAINT checkpoint_position_nonnegative,
    VALIDATE CONSTRAINT checkpoint_parent_complete;

ALTER TABLE history
    VALIDATE CONSTRAINT history_block_position_nonnegative,
    VALIDATE CONSTRAINT history_block_parent_complete;

ALTER TABLE journal
    VALIDATE CONSTRAINT journal_block_position_nonnegative,
    VALIDATE CONSTRAINT journal_block_parent_complete,
    VALIDATE CONSTRAINT journal_previous_checkpoint_complete;

ALTER TABLE checkpoint ALTER COLUMN position SET NOT NULL;
ALTER TABLE history ALTER COLUMN block_position SET NOT NULL;
ALTER TABLE journal ALTER COLUMN block_position SET NOT NULL;

COMMIT;
