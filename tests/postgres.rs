//! Mine vs theirs: a heap page written by a real Postgres with data checksums
//! on, read straight from the cluster's files. Build the cluster first, from
//! the dev shell:
//!
//! ```sh
//! tests/fixtures/make-db.sh
//! ```

mod common;

use std::sync::LazyLock;

use bonsai::{
    TransactionId, XLogRecPtr,
    storage::{
        BLCKSZ_BYTES, BlockNumber,
        checksum::checksum_page,
        page::{Header, Page, PageFlags, PageOffset, PageValidationError},
    },
};
use common::{Facts, read};

/// Block 0 of the table `postgres-heap-page.sql` creates.
struct Fixture {
    page: Page,
    /// The header as pageinspect's `page_header` reads it.
    header: Header,
}

static POSTGRES: LazyLock<Fixture> = LazyLock::new(|| {
    let facts = Facts::read("postgres-heap-page");
    let path: String = facts.get("path");
    let file = read(&format!("data/{path}"));
    let block = file
        .get(..BLCKSZ_BYTES)
        .unwrap_or_else(|| panic!("{path} is shorter than a block"));
    Fixture {
        page: Page {
            bytes: block.try_into().unwrap(),
        },
        header: Header {
            lsn: XLogRecPtr::from_raw(facts.get("lsn")),
            checksum: facts.get("checksum"),
            flags: PageFlags::try_from(facts.get::<u16>("flags")).unwrap(),
            lower: PageOffset(facts.get("lower")),
            upper: PageOffset(facts.get("upper")),
            special: PageOffset(facts.get("special")),
            pagesize_version: facts.get("pagesize_version"),
            prune_xid: TransactionId::from_raw(facts.get("prune_xid")),
        },
    }
});

#[test]
fn header_reads_like_pageinspect() {
    assert_eq!(POSTGRES.page.header(), Ok(POSTGRES.header));
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
    let mut header = page.header().unwrap();
    header.checksum = 0;
    page.set_header(header);
    let sealed = page.try_set_checksum_for_block(BlockNumber(0)).unwrap();
    assert_eq!(sealed.block_number(), BlockNumber(0));
    assert_eq!(sealed.page(), &POSTGRES.page);
}

#[test]
fn postgres_page_at_another_block_fails() {
    assert!(matches!(
        POSTGRES.page.try_validated_for_block(BlockNumber(1)),
        Err(PageValidationError::ChecksumMismatch { block_number: BlockNumber(1), stored, .. })
            if stored == POSTGRES.header.checksum
    ));
}
