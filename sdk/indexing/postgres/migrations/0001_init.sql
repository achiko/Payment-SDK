-- Payment-SDK canonical PostgreSQL schema initializer.
--
-- Two halves with different owners:
--
--   * indexing state (checkpoint, history, movement, output, journal) mirrors
--     what sdk/indexing/redb persists. Scope (chain, network) is part of
--     every primary key, so ONE set of tables serves EVERY indexer — Bitcoin
--     and Ethereum rows live side by side and never collide.
--
--   * payment_wallets is application state. Indexing deliberately stores no
--     filter registry, so the application must be able to re-supply the
--     observed address set after a restart.

BEGIN;

-- ========================================================== indexing state

-- Where each indexer has reached. One row per scope.
CREATE TABLE checkpoint (
    chain            text   NOT NULL,
    network          text   NOT NULL,
    height           bigint NOT NULL,
    hash             bytea  NOT NULL,
    parent_hash      bytea,
    block_timestamp  bigint,
    position         bigint NOT NULL,
    parent_position  bigint,
    PRIMARY KEY (chain, network),
    CONSTRAINT checkpoint_position_nonnegative
        CHECK (position >= 0),
    CONSTRAINT checkpoint_parent_complete
        CHECK (
            (position = 0
                AND parent_position IS NULL
                AND parent_hash IS NULL)
            OR (position > 0
                AND parent_position IS NOT NULL
                AND parent_hash IS NOT NULL
                AND parent_position >= 0
                AND parent_position < position)
        )
);

-- Canonical transactions, address-primary like the redb HISTORY keyspace:
-- "every transaction for this address" is the natural index order.
CREATE TABLE history (
    chain            text    NOT NULL,
    network          text    NOT NULL,
    address          text    NOT NULL,
    height           bigint  NOT NULL,
    transaction_id   text    NOT NULL,
    status           text    NOT NULL CHECK (status IN ('included', 'failed')),
    failure_reason   text,
    block_hash       bytea   NOT NULL,
    block_parent     bytea,
    block_timestamp  bigint,
    fee_asset        text,
    fee_amount       numeric,
    fee_payer        text,
    block_position   bigint NOT NULL,
    block_parent_position bigint,
    PRIMARY KEY (chain, network, address, height, transaction_id),
    CONSTRAINT history_block_position_nonnegative
        CHECK (block_position >= 0),
    CONSTRAINT history_block_parent_complete
        CHECK (
            (block_position = 0
                AND block_parent_position IS NULL
                AND block_parent IS NULL)
            OR (block_position > 0
                AND block_parent_position IS NOT NULL
                AND block_parent IS NOT NULL
                AND block_parent_position >= 0
                AND block_parent_position < block_position)
        )
);

-- Reorg reversal deletes by height; this index makes that cheap.
CREATE INDEX history_by_height ON history (chain, network, height);

-- Value movements. Nested inside TransactionRecord in redb because a KV
-- store cannot query into a blob; a real table here so movements are queryable.
--
-- amount is numeric, not bigint: these are atomic units, and wei reaches 10^77.
-- The SDK serialises Decimal as a base-10 string for the same reason.
CREATE TABLE movement (
    chain            text    NOT NULL,
    network          text    NOT NULL,
    address          text    NOT NULL,
    height           bigint  NOT NULL,
    transaction_id   text    NOT NULL,
    ordinal          int     NOT NULL,
    kind             text    NOT NULL
                     CHECK (kind IN ('transfer', 'input', 'output', 'mint', 'burn')),
    movement_id      text    NOT NULL,
    asset_chain      text    NOT NULL,
    asset            text    NOT NULL,
    amount           numeric NOT NULL,
    from_address     text,
    to_address       text,
    PRIMARY KEY (chain, network, address, height, transaction_id, ordinal)
);

CREATE INDEX movement_by_height ON movement (chain, network, height);

-- Live outputs (UTXO chains). Rows are deleted when spent, so this table is
-- the unspent set, not a log. `evidence` is the chain-native script needed to
-- spend the output, opaque to indexing.
CREATE TABLE output (
    chain            text    NOT NULL,
    network          text    NOT NULL,
    transaction_id   text    NOT NULL,
    output_index     int     NOT NULL,
    address          text    NOT NULL,
    asset_chain      text    NOT NULL,
    asset            text    NOT NULL,
    amount           numeric NOT NULL,
    evidence         bytea   NOT NULL,
    created_at       bigint  NOT NULL,
    coinbase         boolean NOT NULL,
    PRIMARY KEY (chain, network, transaction_id, output_index)
);

CREATE INDEX output_by_address_identity
    ON output (chain, network, address, transaction_id, output_index);
CREATE INDEX output_by_height ON output (chain, network, created_at);

-- Bounded rollback journal, retained `reorg_retention` blocks deep.
--
-- Much smaller than the redb equivalent, which must carry history_keys and
-- remove_output_keys because a KV store cannot delete by predicate. Here those
-- are DELETE ... WHERE height = $1. Only outputs a reorged block SPENT need
-- recording: nothing else can reconstruct them.
CREATE TABLE journal (
    chain                       text   NOT NULL,
    network                     text   NOT NULL,
    height                      bigint NOT NULL,
    block_hash                  bytea  NOT NULL,
    -- BlockSelector::Height reads retained blocks from here and must return a
    -- complete BlockRef, so parent and timestamp are stored, not just the hash.
    block_parent                bytea,
    block_timestamp             bigint,
    previous_checkpoint_height  bigint,
    previous_checkpoint_hash    bytea,
    previous_checkpoint_parent  bytea,
    previous_checkpoint_time    bigint,
    block_position              bigint NOT NULL,
    block_parent_position       bigint,
    previous_checkpoint_position bigint,
    previous_checkpoint_parent_position bigint,
    PRIMARY KEY (chain, network, height),
    CONSTRAINT journal_block_position_nonnegative
        CHECK (block_position >= 0),
    CONSTRAINT journal_block_parent_complete
        CHECK (
            (block_position = 0
                AND block_parent_position IS NULL
                AND block_parent IS NULL)
            OR (block_position > 0
                AND block_parent_position IS NOT NULL
                AND block_parent IS NOT NULL
                AND block_parent_position >= 0
                AND block_parent_position < block_position)
        ),
    CONSTRAINT journal_previous_checkpoint_complete
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
        )
);

-- Outputs a journalled block SPENT, kept so a reorg can put them back.
--
-- The only rollback data that cannot be derived: history and created outputs
-- are deleted by height, but a spent output is gone from the live set and
-- nothing else holds its amount, script, or coinbase flag.
CREATE TABLE journal_output (
    chain            text    NOT NULL,
    network          text    NOT NULL,
    height           bigint  NOT NULL,
    transaction_id   text    NOT NULL,
    output_index     int     NOT NULL,
    address          text    NOT NULL,
    asset_chain      text    NOT NULL,
    asset            text    NOT NULL,
    amount           numeric NOT NULL,
    evidence         bytea   NOT NULL,
    created_at       bigint  NOT NULL,
    coinbase         boolean NOT NULL,
    PRIMARY KEY (chain, network, height, transaction_id, output_index),
    FOREIGN KEY (chain, network, height)
        REFERENCES journal (chain, network, height) ON DELETE CASCADE
);

-- ======================================================= application state

-- Wallets this process generated and must re-register for observation after a
-- restart. `start_height` is the address birthday — the checkpoint at creation
-- time — which is what `Wallets::import` needs to anchor it correctly.
CREATE TABLE payment_wallets (
    id            text        PRIMARY KEY,
    chain         text        NOT NULL,
    network       text        NOT NULL,
    address       text        NOT NULL,
    start_height  bigint      NOT NULL,
    secret        bytea       NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    UNIQUE (chain, network, address)
);

-- Startup restore reads the whole set for one scope.
CREATE INDEX payment_wallets_by_scope ON payment_wallets (chain, network);

COMMIT;
