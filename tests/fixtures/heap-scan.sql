-- A table spanning several pages, for sequential scans, with the columns of
-- bonsai's `Row`. Values cover every way a column can be stored:
--
-- - `payload` mixes NULLs, empty strings, non-ASCII text, and lengths on
--   either side of 126 bytes, the most a 1-byte varlena header can hold.
-- - `non_null_payload` mixes empty strings, short values and values longer
--   than 126 bytes. After a short `payload` that ends at an offset that isn't
--   a multiple of 4, a long one starts with padding, and a short one without.
--
-- No row comes near the 2 KB at which Postgres would compress a value or move
-- it to TOAST.
CREATE TABLE heap_scan (
    id int8 PRIMARY KEY,
    payload text,
    non_null_payload text NOT NULL
);
INSERT INTO heap_scan
SELECT i,
    CASE
        WHEN i % 10 = 0 THEN NULL
        WHEN i % 10 = 5 THEN ''
        WHEN i % 10 = 7 THEN 'café n°' || i
        ELSE repeat(md5(i::text), 1 + i % 20)
    END,
    CASE
        WHEN i % 3 = 0 THEN ''
        WHEN i % 3 = 1 THEN 'n' || i
        ELSE repeat(md5((-i)::text), 5)
    END
FROM generate_series(1, 1000) AS i;

-- Sets the hint bits, so that every row reads as committed without
-- Postgres's commit log.
SELECT count(*) FROM heap_scan \gset

SELECT pg_relation_filepath('heap_scan') AS path;

-- What the tests compare against, written next to the facts as COPY's text
-- format: one line per row, tab-separated, `\N` for NULL. COPY needs an
-- absolute path, so it starts from the data directory.
SELECT current_setting('data_directory') || '/../heap-scan.rows' AS rows_file,
    current_setting('data_directory') || '/../heap-scan.items' AS items_file \gset

COPY (
    SELECT ctid, id, payload, non_null_payload FROM heap_scan ORDER BY ctid
) TO :'rows_file';

-- Every line pointer and tuple header, as pageinspect reads them.
CREATE EXTENSION IF NOT EXISTS pageinspect;
COPY (
    SELECT blkno, lp, lp_off, lp_flags, lp_len,
        t_xmin, t_xmax, t_field3, t_ctid, t_infomask2, t_infomask, t_hoff
    FROM generate_series(0, pg_relation_size('heap_scan') / 8192 - 1) AS blkno,
        heap_page_items(get_raw_page('heap_scan', blkno))
    ORDER BY blkno, lp
) TO :'items_file';
