//! Mine vs theirs: a heap page written by a real Postgres with data checksums
//! on, read straight from the cluster's files. Build the cluster first, from
//! the dev shell:
//!
//! ```sh
//! tests/fixtures/make-db.sh
//! ```

use std::collections::HashMap;
use std::fmt::Debug;
use std::fs;
use std::str::FromStr;
use std::sync::LazyLock;

use bonsai::storage::checksum::checksum_page;
use bonsai::storage::page::{Header, Page, PageError};
use bonsai::storage::{BLCKSZ, BlockNumber};

/// The cluster `make-db.sh` builds, and the facts it saves next to it.
const POSTGRES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/postgres");

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
        .get(..BLCKSZ)
        .unwrap_or_else(|| panic!("{path} is shorter than a block"));
    Fixture {
        page: Page {
            bytes: block.try_into().unwrap(),
        },
        header: Header {
            lsn: facts.get("lsn"),
            checksum: facts.get("checksum"),
            flags: facts.get("flags"),
            lower: facts.get("lower"),
            upper: facts.get("upper"),
            special: facts.get("special"),
            pagesize_version: facts.get("pagesize_version"),
            prune_xid: facts.get("prune_xid"),
        },
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

/// What a fixture script printed: one `name=value` line per value.
struct Facts(HashMap<String, String>);

impl Facts {
    fn read(fixture: &str) -> Self {
        let text = String::from_utf8(read(&format!("{fixture}.facts"))).unwrap();
        let values = text
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect();
        Facts(values)
    }

    fn get<T: FromStr<Err: Debug>>(&self, name: &str) -> T {
        let value = self
            .0
            .get(name)
            .unwrap_or_else(|| panic!("no {name} in the facts"));
        value.parse().unwrap()
    }
}

/// Reads a file under `postgres/`. Panics if it isn't there.
fn read(path: &str) -> Vec<u8> {
    let path = format!("{POSTGRES_DIR}/{path}");
    fs::read(&path).unwrap_or_else(|err| {
        panic!("could not read {path}: {err}. Build the cluster with `tests/fixtures/make-db.sh`.")
    })
}
