-- Delete movements by predicate instead of by cascade.
--
-- `movement` referenced `history` only to inherit ON DELETE CASCADE for reorg
-- reversal. That cascade is used once per reorg; the foreign key is checked on
-- every inserted movement, and movements are the highest-volume row an indexer
-- writes — one per movement per address a transaction touched.
--
-- Measured on this schema, inserting 80k movement rows:
--
--     without the foreign key      264 ms
--     with the foreign key       1_138 ms
--
-- The check costs more than four times the insert itself, because each row
-- probes the parent index and takes a KEY SHARE lock on the history row.
--
-- Reversal now deletes movements the same way it already deletes history and
-- created outputs: by (chain, network, height), which is derivable from the
-- journal without a parent link. This index makes that delete an index scan,
-- mirroring history_by_height.
--
-- `journal_output` keeps its foreign key deliberately. It is bounded by the
-- retention window rather than by chain volume, its rows all point at one
-- parent, and the same measurement puts its check at well under a millisecond
-- per block — so there the cascade is worth what it costs.

BEGIN;

CREATE INDEX movement_by_height ON movement (chain, network, height);

ALTER TABLE movement DROP CONSTRAINT movement_chain_network_address_height_transaction_id_fkey;

COMMIT;
