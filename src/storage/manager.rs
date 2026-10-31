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

/// How many blocks fit in a 1GB segment file.
pub const RELSEG_SIZE: BlockCount = BlockCount(1024 * 1024 * 1024 / (BLCKSZ as u32));

pub struct InvalidBlockNumber;

#[derive(Clone, Copy, Hash, Eq, PartialEq)]
pub struct RelFileNumber(u32);

#[derive(Clone, Copy, Hash, Eq, PartialEq)]
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

#[derive(Clone, Copy, Hash, Eq, PartialEq)]
pub struct ForkIdentifier {
    pub rel: RelFileNumber,
    pub kind: ForkKind,
}

#[derive(Clone, Copy)]
pub struct BlockIdentifier {
    pub fork: ForkIdentifier,
    pub block_number: BlockNumber,
}

pub struct BlockError {
    pub block: BlockIdentifier,
    pub inner: BlockErrorInner,
}

pub struct ForkError {
    pub fork: ForkIdentifier,
    pub inner: ForkErrorInner,
}

pub enum ForkErrorInner {
    Io(io::Error),
    SegmentFileInconsistentlyBig {
        path: PathBuf,
        file_size: u64,
        n_blocks_per_segment: BlockCount,
    },
}

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

    pub fn with_segment_size(
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
        let mut segments = Vec::new();
        loop {
            let segno = segments.len() as u32;
            let path = self.path(fork, segno);
            let file = match File::open(&path) {
                Ok(file) => file,
                // The previous segment was full and nothing comes after it.
                Err(err) if segno > 0 && err.kind() == io::ErrorKind::NotFound => {
                    return Ok(OpenFork {
                        segments,
                        nb_blocks: BlockCount(segno * self.n_block_per_segment.0),
                    });
                }
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
            let blocks = u32::try_from(file_size / (BLCKSZ as u64)).map_err(|_| ForkError {
                fork,
                inner: ForkErrorInner::SegmentFileInconsistentlyBig {
                    path: path.clone(),
                    file_size,
                    n_blocks_per_segment: self.n_block_per_segment,
                },
            })?;
            if blocks > self.n_block_per_segment.0 {
                return Err(ForkError {
                    fork,
                    inner: ForkErrorInner::SegmentFileInconsistentlyBig {
                        path,
                        file_size,
                        n_blocks_per_segment: self.n_block_per_segment,
                    },
                });
            }
            segments.push(file);
            if blocks < self.n_block_per_segment.0 {
                return Ok(OpenFork {
                    segments,
                    nb_blocks: BlockCount(segno * self.n_block_per_segment.0 + blocks),
                });
            }
        }
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

/// A fork whose segment files are open.
struct OpenFork {
    segments: Vec<File>,
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
