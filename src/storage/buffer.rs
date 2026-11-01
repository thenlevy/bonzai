//! Where readers get their pages from. Postgres's buffer manager, `bufmgr.c`.
//!
//! Pages are not cached: every new reader requires fetching the page from the
//! disk. This will be addressed in future versions.

use std::marker::PhantomData;

use super::{
    BLCKSZ,
    manager::{BlockError, BlockIdentifier, StorageManager},
    page::{Page, PageValidationError},
};

pub struct BufferManager {
    smgr: StorageManager,
}

#[derive(Debug)]
pub enum ReadError {
    Block(BlockError),
    Page(PageValidationError),
}

/// A page read through the [`BufferManager`].
///
/// It internally owns the copy of the page, meaning that the bytes are dropped from memory when
/// guard is dropped. When the buffer manager caches pages, this will no longer be the case.
/// Instead, the pages will be owned by the buffer manager, and the guard will borrow from it.
/// In order to prepare for this future change, we already bound the lifetime of the guard to the
/// buffer manager.
pub struct PageGuard<'a> {
    page: Box<Page>,
    block: BlockIdentifier,
    _manager: PhantomData<&'a BufferManager>,
}

impl BufferManager {
    pub fn new(smgr: StorageManager) -> Self {
        Self { smgr }
    }

    /// Read `block`, and returns a guarded page after validating it.
    pub fn validated_read(&self, block: BlockIdentifier) -> Result<PageGuard<'_>, ReadError> {
        let mut page = Box::new(Page { bytes: [0; BLCKSZ] });
        self.smgr.read(block, &mut page).map_err(ReadError::Block)?;
        page.try_validated_for_block(block.block_number)
            .map_err(ReadError::Page)?;
        Ok(PageGuard {
            page,
            block,
            _manager: PhantomData,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::storage::{
        BlockNumber,
        manager::{ForkIdentifier, ForkKind, RelFileNumber},
    };

    use std::fs;

    const MAIN: ForkIdentifier = ForkIdentifier {
        rel: RelFileNumber(16384),
        kind: ForkKind::Main,
    };

    fn block(block_number: u32) -> BlockIdentifier {
        BlockIdentifier {
            fork: MAIN,
            block_number: BlockNumber(block_number),
        }
    }

    fn sealed_page(block_number: u32) -> Page {
        let mut page = Page { bytes: [0; BLCKSZ] };
        Page::init(&mut page, 0);
        let sealed = page
            .try_set_checksum_for_block(BlockNumber(block_number))
            .unwrap();
        sealed.page().clone()
    }

    /// A buffer manager over a main fork made of `pages`.
    fn buffers(dir: &tempfile::TempDir, pages: &[Page]) -> BufferManager {
        let bytes: Vec<u8> = pages.iter().flat_map(|page| page.bytes).collect();
        fs::write(dir.path().join("16384"), bytes).unwrap();
        BufferManager::new(StorageManager::open(dir.path()).unwrap())
    }

    #[test]
    fn a_page_in_the_wrong_block_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let buffers = buffers(&dir, &[sealed_page(1)]);
        assert!(matches!(
            buffers.validated_read(block(0)),
            Err(ReadError::Page(
                PageValidationError::ChecksumMismatch { .. }
            ))
        ));
    }
}
