//! The storage manager reading files a real Postgres wrote: every fork of the
//! fixture cluster's `postgres` database.

use std::fs;
use std::path::Path;

use bonsai::storage::manager::{
    BlockError, BlockErrorInner, BlockIdentifier, ForkIdentifier, ForkKind, RelFileNumber,
    StorageManager,
};
use bonsai::storage::page::Page;
use bonsai::storage::{BLCKSZ, BlockCount, BlockNumber};

/// The `postgres` database (OID 5) of the cluster `tests/fixtures/make-db.sh`
/// builds.
const FIXTURE_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/postgres/data/base/5");

/// Every block of every fork in the fixture database (the system catalogs,
/// their indexes and free space and visibility maps, and the tables the
/// fixture scripts create) reads back as the bytes in its file, with a
/// checksum that matches its block number.
#[test]
fn every_fork_of_the_fixture_database_reads_back() {
    let smgr = StorageManager::open(FIXTURE_DB).unwrap_or_else(|err| {
        panic!("{err:#}. Build the cluster with `tests/fixtures/make-db.sh`.")
    });
    let mut forks = 0;
    for entry in fs::read_dir(FIXTURE_DB).unwrap() {
        let path = entry.unwrap().path();
        let Some(fork) = fork_of(&path) else {
            continue;
        };
        let file = fs::read(&path).unwrap();
        // So that each fork is a single file, and block `n` sits at `n * BLCKSZ` in it.
        assert!(
            !path.with_extension("1").exists(),
            "{path:?} has a second segment"
        );
        let nb_blocks = (file.len() / BLCKSZ) as u32;
        for (n, expected) in (0..).zip(file.chunks_exact(BLCKSZ)) {
            let page = read(&smgr, fork, n).unwrap();
            assert!(page.bytes == expected, "{path:?}: block {n} differs");
            if let Err(err) = page.try_validated_for_block(BlockNumber(n)) {
                panic!("{path:?}: {err}");
            }
        }
        assert!(
            is_past_the_end(&read(&smgr, fork, nb_blocks), nb_blocks),
            "{path:?}"
        );
        forks += 1;
    }
    // About 300 in a fresh Postgres 17 cluster.
    assert!(forks > 100, "only {forks} forks in {FIXTURE_DB}");
}

/// The fork whose first segment `path` is, from its name: `16385` is the
/// main fork of relation 16385, `16385_fsm` its free space map. `None` for
/// other files, such as `pg_filenode.map` or a later segment like `16385.1`.
fn fork_of(path: &Path) -> Option<ForkIdentifier> {
    let name = path.file_name()?.to_str()?;
    let (rel, kind) = match name.split_once('_') {
        None => (name, ForkKind::Main),
        Some((rel, "fsm")) => (rel, ForkKind::FreeSpaceMap),
        Some((rel, "vm")) => (rel, ForkKind::VisibilityMap),
        Some((rel, "init")) => (rel, ForkKind::Init),
        Some(_) => return None,
    };
    Some(ForkIdentifier {
        rel: RelFileNumber(rel.parse().ok()?),
        kind,
    })
}

fn read(smgr: &StorageManager, fork: ForkIdentifier, block: u32) -> Result<Page, BlockError> {
    let block = BlockIdentifier {
        fork,
        block_number: BlockNumber(block),
    };
    let mut page = Page { bytes: [0; BLCKSZ] };
    smgr.read(block, &mut page).map(|()| page)
}

fn is_past_the_end(result: &Result<Page, BlockError>, fork_size: u32) -> bool {
    matches!(
        result,
        Err(BlockError {
            inner: BlockErrorInner::BlockDoesNotExist { fork_size: size },
            ..
        }) if *size == BlockCount(fork_size)
    )
}
