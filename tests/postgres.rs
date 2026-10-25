//! Mine vs theirs: a heap page written by a real Postgres with data checksums
//! on. Generate it first, from the dev shell:
//!
//! ```sh
//! tests/fixtures/make-bin.sh tests/fixtures/postgres-heap-page.sql
//! ```

use std::fs;
use std::sync::LazyLock;

use bonsai::storage::checksum::checksum_page;
use bonsai::storage::page::{Header, Page, PageError};
use bonsai::storage::{BLCKSZ, BlockNumber};

/// Where `make-bin.sh` saves what the fixture scripts print.
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/fixtures");

/// What `postgres-heap-page.sql` printed.
struct Fixture {
    page: Page,
    /// The header as pageinspect's `page_header` reads it.
    header: Header,
}

static POSTGRES: LazyLock<Fixture> = LazyLock::new(|| {
    let bytes = read("postgres-heap-page.bin");
    let size = bytes.len();
    let bytes = bytes
        .try_into()
        .unwrap_or_else(|_| panic!("expected one {BLCKSZ}-byte block, got {size} bytes"));
    let header = String::from_utf8(read("postgres-heap-page.header")).unwrap();
    Fixture {
        page: Page { bytes },
        header: parse_header(header.trim_end()),
    }
});

#[test]
fn header_reads_like_pageinspect() {
    assert_eq!(POSTGRES.page.header(), POSTGRES.header);
}

#[test]
fn checksum_matches_postgres() {
    assert_ne!(POSTGRES.header.checksum, 0, "checksums are off");
    assert_eq!(
        checksum_page(&POSTGRES.page, BlockNumber(0)),
        POSTGRES.header.checksum
    );
    assert!(
        POSTGRES
            .page
            .try_validated_for_block(BlockNumber(0))
            .is_ok()
    );
}

#[test]
fn sealing_writes_back_what_postgres_wrote() {
    let mut page = POSTGRES.page.clone();
    let mut header = page.header();
    header.checksum = 0;
    page.set_header(header);
    let sealed = page.try_set_checksum_for_block(BlockNumber(0)).unwrap();
    assert_eq!(sealed.block(), BlockNumber(0));
    assert_eq!(sealed.page(), &POSTGRES.page);
}

#[test]
fn postgres_page_at_another_block_fails() {
    assert!(matches!(
        POSTGRES.page.try_validated_for_block(BlockNumber(1)),
        Err(PageError::ChecksumMismatch { block: BlockNumber(1), stored, .. })
            if stored == POSTGRES.header.checksum
    ));
}

/// Reads a file `make-bin.sh` saved. Panics if it isn't there.
fn read(name: &str) -> Vec<u8> {
    let path = format!("{FIXTURES}/{name}");
    fs::read(&path).unwrap_or_else(|err| {
        panic!(
            "could not read {path}: {err}. Generate it with \
             `tests/fixtures/make-bin.sh tests/fixtures/postgres-heap-page.sql`."
        )
    })
}

/// Parses the fields `page_header` returned, separated by `|`, in the order
/// `Header` declares them.
fn parse_header(line: &str) -> Header {
    let mut fields = line.split('|');
    let mut next = || fields.next().expect("too few fields");
    let header = Header {
        lsn: next().parse().unwrap(),
        checksum: next().parse().unwrap(),
        flags: next().parse().unwrap(),
        lower: next().parse().unwrap(),
        upper: next().parse().unwrap(),
        special: next().parse().unwrap(),
        pagesize_version: next().parse().unwrap(),
        prune_xid: next().parse().unwrap(),
    };
    assert!(fields.next().is_none(), "too many fields: {line}");
    header
}
