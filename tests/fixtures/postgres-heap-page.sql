-- Block 0 of a three-row heap table, as Postgres stores it with data checksums
-- on. make-bin.sh runs this in a throwaway cluster and saves what it prints,
-- the block in hex and then its header as pageinspect reads it, for
-- tests/postgres.rs and hexyl.

CREATE TABLE t (id int8 GENERATED ALWAYS AS IDENTITY, payload text);
INSERT INTO t (payload) VALUES ('Hello, World!'), ('Bonjour Bonsai'), ('An other row');

-- A page gets its checksum when it is written out, so read the block back
-- from the file after a checkpoint rather than from shared buffers.
CHECKPOINT;
SELECT pg_read_binary_file(pg_relation_filepath('t'), 0, 8192) AS page \gset
SELECT encode(:'page', 'hex');

-- page_header returns the LSN as text and the checksum as a signed smallint,
-- so convert both to plain numbers.
CREATE EXTENSION pageinspect;
SELECT lsn - '0/0', checksum::int & 65535, flags, lower, upper, special,
    pagesize | version, prune_xid
FROM page_header(:'page');
