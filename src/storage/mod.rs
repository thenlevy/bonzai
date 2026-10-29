//! Pages and the files they live in.
//!
//! See [Database File Layout](https://www.postgresql.org/docs/current/storage-file-layout.html).

use std::fmt;

pub mod checksum;
pub mod page;

/// Size of a page, and of every block in a relation file. Postgres's `BLCKSZ`.
pub const BLCKSZ: usize = 8192;

/// Position of a block within one fork of a relation, counting from 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockNumber(pub u32);

impl fmt::Display for BlockNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}
