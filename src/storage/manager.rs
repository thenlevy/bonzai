//! Maps (relation, fork, block) to a file and an offset, and handles storing and retrieving blocks.
//! Postgres: `smgr.c` and `md.c`.

use super::{BLCKSZ, BlockCount, BlockNumber, page::Page};

use anyhow::Context;

use std::{
    collections::HashMap,
    fmt::Write,
    fs::{self, File},
    io,
    os::unix::fs::FileExt,
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Clone, Copy, Debug)]
pub struct BlockIdentifier {
    pub fork: ForkIdentifier,
    pub block_number: BlockNumber,
}

#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub struct ForkIdentifier {
    pub rel: RelFileNumber,
    pub kind: ForkKind,
}

#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub struct RelFileNumber(pub u32);

#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub enum ForkKind {
    Main = 0,
    FreeSpaceMap = 1,
    VisibilityMap = 2,
    Init = 3,
}

pub struct StorageManager {
    dir: PathBuf,
    // Not necessarily equal to RELSEG_SIZE. This allows emulating smaller segment sizes for
    // testing.
    n_block_per_segment: BlockCount,
    opened_forks: Mutex<HashMap<ForkIdentifier, Arc<OpenFork>>>,
}

#[derive(Debug)]
pub struct BlockError {
    pub block: BlockIdentifier,
    pub inner: BlockErrorInner,
}

#[derive(Debug)]
pub struct ForkError {
    pub fork: ForkIdentifier,
    pub inner: ForkErrorInner,
}

#[derive(Debug)]
pub enum ForkErrorInner {
    Io(io::Error),
    SegmentFileInconsistentlyBig {
        path: PathBuf,
        file_size: u64,
        n_blocks_per_segment: BlockCount,
    },
}

#[derive(Debug)]
pub enum BlockErrorInner {
    BlockDoesNotExist { fork_size: BlockCount },
    Io(io::Error),
    ForkError(ForkError),
}

impl StorageManager {
    /// Opens `dir`, the directory holding the relation files: `base/<database
    /// OID>` in a Postgres cluster.
    pub fn open(dir: impl Into<PathBuf>) -> anyhow::Result<Self> {
        Self::with_segment_size(dir, RELSEG_SIZE)
    }

    /// Fetch the block from the disk and store its bits into `page`.
    pub fn read(&self, block: BlockIdentifier, page: &mut Page) -> Result<(), BlockError> {
        let fork = self.get_fork(block.fork).map_err(|err| BlockError {
            block,
            inner: BlockErrorInner::ForkError(err),
        })?;
        if !fork.nb_blocks.contains(block.block_number) {
            return Err(BlockError {
                block,
                inner: BlockErrorInner::BlockDoesNotExist {
                    fork_size: fork.nb_blocks,
                },
            });
        }
        let Position {
            segno,
            offset_bytes,
        } = self.position(block.block_number);
        fork.segments[segno as usize]
            .read_exact_at(page, offset_bytes)
            .map_err(|err| BlockError {
                block,
                inner: BlockErrorInner::Io(err),
            })
    }

    fn with_segment_size(
        dir: impl Into<PathBuf>,
        n_block_per_segment: BlockCount,
    ) -> anyhow::Result<Self> {
        assert!(
            n_block_per_segment.0 > 0,
            "segments must hold at least one block"
        );
        let dir = dir.into();
        fs::read_dir(&dir)
            .with_context(|| format!("could not open directory {}", dir.display()))?;
        Ok(Self {
            dir,
            n_block_per_segment,
            opened_forks: Mutex::new(HashMap::new()),
        })
    }

    fn get_fork(&self, fork: ForkIdentifier) -> Result<Arc<OpenFork>, ForkError> {
        let mut forks = self.opened_forks.lock().unwrap();
        if let Some(open) = forks.get(&fork) {
            return Ok(Arc::clone(open));
        }
        let open = Arc::new(self.load_fork(fork)?);
        forks.insert(fork, Arc::clone(&open));
        Ok(open)
    }

    fn load_fork(&self, fork: ForkIdentifier) -> Result<OpenFork, ForkError> {
        let empty = OpenFork {
            segments: Vec::new(),
            nb_blocks: BlockCount(0),
        };
        let mut previous_full = true;
        (0..)
            // Stops at the first missing segment, or after the first one that
            // isn't full: only the last segment can be partly full.
            .map_while(|segno| {
                if !previous_full {
                    return None;
                }
                let segment = self.open_segment(fork, segno).transpose()?;
                previous_full = segment
                    .as_ref()
                    .is_ok_and(|segment| segment.nb_blocks == self.n_block_per_segment);
                Some(segment)
            })
            .try_fold(empty, |mut open, segment| {
                let segment = segment?;
                open.segments.push(segment.file);
                open.nb_blocks.0 += segment.nb_blocks.0;
                Ok(open)
            })
    }

    /// Opens segment `segno` of `fork` and counts its blocks. `None` if there
    /// is no such segment, except for segment 0: a missing fork is an error.
    fn open_segment(&self, fork: ForkIdentifier, segno: u32) -> Result<Option<Segment>, ForkError> {
        let path = self.path(fork, segno);
        let file = match File::open(&path) {
            Ok(file) => file,
            // The previous segment was full and nothing comes after it.
            Err(err) if segno > 0 && err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(ForkError {
                    fork,
                    inner: ForkErrorInner::Io(err),
                });
            }
        };
        // A partly written block at the end, left by a crash, doesn't
        // count, as in `mdnblocks`.
        let file_size = file
            .metadata()
            .map_err(|err| ForkError {
                fork,
                inner: err.into(),
            })?
            .len();
        let blocks = file_size / (BLCKSZ as u64);
        if blocks > u64::from(self.n_block_per_segment.0) {
            return Err(ForkError {
                fork,
                inner: ForkErrorInner::SegmentFileInconsistentlyBig {
                    path,
                    file_size,
                    n_blocks_per_segment: self.n_block_per_segment,
                },
            });
        }
        Ok(Some(Segment {
            file,
            nb_blocks: BlockCount(blocks as u32),
        }))
    }

    /// Path to the file holding the segment `segno` of `fork`.
    fn path(&self, fork: ForkIdentifier, segno: u32) -> PathBuf {
        let mut name = fork.rel.0.to_string();
        match fork.kind {
            ForkKind::Main => {}
            ForkKind::FreeSpaceMap => {
                name.push_str("_fsm");
            }
            ForkKind::VisibilityMap => {
                name.push_str("_vm");
            }
            ForkKind::Init => {
                name.push_str("_init");
            }
        }
        if segno > 0 {
            write!(&mut name, ".{segno}").unwrap();
        }
        self.dir.join(name)
    }

    fn position(&self, block: BlockNumber) -> Position {
        let segno = block.0 / self.n_block_per_segment.0;
        let offset = u64::from(block.0 % self.n_block_per_segment.0) * BLCKSZ as u64;
        Position {
            segno,
            offset_bytes: offset,
        }
    }
}

/// How many blocks fit in a 1GB segment file.
pub const RELSEG_SIZE: BlockCount = BlockCount(1024 * 1024 * 1024 / (BLCKSZ as u32));

/// A fork whose segment files are open.
struct OpenFork {
    segments: Vec<File>,
    nb_blocks: BlockCount,
}

/// One open segment file of a fork.
struct Segment {
    file: File,
    nb_blocks: BlockCount,
}

struct Position {
    segno: u32,
    offset_bytes: u64,
}

impl From<io::Error> for ForkErrorInner {
    fn from(err: io::Error) -> Self {
        ForkErrorInner::Io(err)
    }
}

impl From<io::Error> for BlockErrorInner {
    fn from(err: io::Error) -> Self {
        BlockErrorInner::Io(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;

    const MAIN: ForkIdentifier = ForkIdentifier {
        rel: RelFileNumber(16384),
        kind: ForkKind::Main,
    };

    /// Writes a file of `count` blocks into `dir`, block `i` filled with the
    /// byte `first + i`.
    fn write_blocks(dir: &Path, name: &str, first: u8, count: u8) {
        let bytes: Vec<u8> = (first..first + count)
            .flat_map(|byte| [byte; BLCKSZ])
            .collect();
        fs::write(dir.join(name), bytes).unwrap();
    }

    /// A storage manager whose segments hold two blocks, so that a few blocks
    /// are enough to span several segments.
    fn open_with_small_segments(dir: &Path) -> StorageManager {
        StorageManager::with_segment_size(dir, BlockCount(2)).unwrap()
    }

    fn nb_blocks(smgr: &StorageManager) -> BlockCount {
        smgr.get_fork(MAIN).unwrap().nb_blocks
    }

    fn read(smgr: &StorageManager, block_number: u32) -> Result<Page, BlockError> {
        let block = BlockIdentifier {
            fork: MAIN,
            block_number: BlockNumber(block_number),
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

    #[test]
    fn blocks_are_found_across_segments() {
        let dir = tempfile::tempdir().unwrap();
        write_blocks(dir.path(), "16384", 0, 2);
        write_blocks(dir.path(), "16384.1", 2, 2);
        write_blocks(dir.path(), "16384.2", 4, 1);
        let smgr = open_with_small_segments(dir.path());
        assert_eq!(nb_blocks(&smgr), BlockCount(5));
        for block_number in 0..5 {
            let page = read(&smgr, block_number).unwrap();
            assert!(
                page.bytes == [block_number as u8; BLCKSZ],
                "block {block_number}"
            );
        }
        assert!(is_past_the_end(&read(&smgr, 5), 5));
    }

    #[test]
    fn a_fork_can_end_with_a_full_segment() {
        let dir = tempfile::tempdir().unwrap();
        write_blocks(dir.path(), "16384", 0, 2);
        write_blocks(dir.path(), "16384.1", 2, 2);
        let smgr = open_with_small_segments(dir.path());
        assert_eq!(nb_blocks(&smgr), BlockCount(4));
        assert!(is_past_the_end(&read(&smgr, 4), 4));
    }

    #[test]
    fn an_empty_fork_has_no_blocks() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("16384"), []).unwrap();
        let smgr = open_with_small_segments(dir.path());
        assert_eq!(nb_blocks(&smgr), BlockCount(0));
        assert!(is_past_the_end(&read(&smgr, 0), 0));
    }

    /// As in `mdnblocks`, counting stops at the first segment that isn't
    /// full, whatever files come after it.
    #[test]
    fn segments_after_a_partial_one_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        write_blocks(dir.path(), "16384", 0, 1);
        write_blocks(dir.path(), "16384.1", 9, 2);
        let smgr = open_with_small_segments(dir.path());
        assert_eq!(nb_blocks(&smgr), BlockCount(1));
    }

    #[test]
    fn a_partial_block_at_the_end_does_not_count() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = vec![0; BLCKSZ];
        bytes.extend([1; 100]);
        fs::write(dir.path().join("16384"), bytes).unwrap();
        let smgr = StorageManager::open(dir.path()).unwrap();
        assert_eq!(nb_blocks(&smgr), BlockCount(1));
        assert!(is_past_the_end(&read(&smgr, 1), 1));
    }

    #[test]
    fn a_missing_fork_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let smgr = open_with_small_segments(dir.path());
        let result = read(&smgr, 0);
        assert!(
            matches!(
                &result,
                Err(BlockError {
                    inner: BlockErrorInner::ForkError(ForkError {
                        inner: ForkErrorInner::Io(err),
                        ..
                    }),
                    ..
                }) if err.kind() == io::ErrorKind::NotFound
            ),
            "{result:?}"
        );
    }

    #[test]
    fn a_segment_bigger_than_the_segment_size_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write_blocks(dir.path(), "16384", 0, 3);
        let smgr = open_with_small_segments(dir.path());
        let result = smgr.get_fork(MAIN).map(|_| ());
        assert!(
            matches!(
                &result,
                Err(ForkError {
                    inner: ForkErrorInner::SegmentFileInconsistentlyBig {
                        file_size,
                        n_blocks_per_segment: BlockCount(2),
                        ..
                    },
                    ..
                }) if *file_size == 3 * BLCKSZ as u64
            ),
            "{result:?}"
        );
    }
}
