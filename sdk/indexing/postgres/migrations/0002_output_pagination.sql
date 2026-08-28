-- Order live outputs by the identity the cursor pages on.
--
-- `Outputs::list` walks an address's unspent set ordered by
-- (transaction_id, output_index), resuming from the cursor as a row-value
-- range. `output_by_address` stops at `address`, so the planner had to read
-- every output the address owns and sort it to return one page — work that
-- grows with the wallet's UTXO count on every page, not with the page size.
--
-- Extending the index through the ordering columns lets the index supply the
-- order: the scan starts at the cursor and stops after LIMIT rows.
--
-- This supersedes output_by_address, which is a strict prefix of it and is
-- dropped rather than left to be maintained on every insert and delete.

BEGIN;

CREATE INDEX output_by_address_identity
    ON output (chain, network, address, transaction_id, output_index);

DROP INDEX output_by_address;

COMMIT;
