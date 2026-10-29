CREATE TABLE t (id int8 GENERATED ALWAYS AS IDENTITY, payload text);
INSERT INTO t (payload) VALUES ('Hello, World!'), ('Bonjour Bonsai'), ('An other row');

-- This will set the hint bits on the rows. This is needed to ensure this bits
-- are already set when dumping the page's header bellow. Otherwise the bits would
-- be set after the dump and the dumped data would not match the actual fork state.
SELECT count(*) FROM t \gset

-- Commit the fork state to disk, which sets the checksum on the page.
CHECKPOINT;

-- Dump the page's header in human readable format.
CREATE EXTENSION pageinspect;
SELECT pg_relation_filepath('t') AS path,
    lsn - '0/0' AS lsn,
    checksum::int & 65535 AS checksum,
    flags,
    lower,
    upper,
    special,
    pagesize | version AS pagesize_version,
    prune_xid
FROM page_header(pg_read_binary_file(pg_relation_filepath('t'), 0, 8192));
