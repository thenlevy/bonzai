//! See [bufpage.h](https://github.com/postgres/postgres/blob/master/src/include/storage/bufpage.h)
//! and [bufpage.c](https://github.com/postgres/postgres/blob/master/src/backend/storage/page/bufpage.c).
//!
//! # Page layout
//! ```text
//! +--------+---------------+----------------+--------------+---------+
//! | header | line pointers |→  free space  ←| tuple data   | special |
//! +--------+---------------+----------------+--------------+---------+
//! 0        24          pd_lower         pd_upper     pd_special    8192
//! ```
//!
//! Fields are little-endian. Postgres uses the machine's byte order, which is
//! little-endian on x86-64 and ARM64, so there the bytes match Postgres's.

use std::{
    borrow::Cow,
    fmt,
    ops::{Deref, DerefMut, Range},
};

use super::{BLCKSZ, BlockNumber, checksum::checksum_page};

/// An 8 KB block, exactly as stored on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub bytes: [u8; BLCKSZ],
}

impl Deref for Page {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

impl DerefMut for Page {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.bytes
    }
}

pub struct ValidatedPage<'a> {
    page: Cow<'a, Page>,
    block: BlockNumber,
}

impl ValidatedPage<'_> {
    pub fn page(&self) -> &Page {
        &self.page
    }

    /// The block the page's checksum was computed for.
    pub fn block(&self) -> BlockNumber {
        self.block
    }
}

pub const HEADER_SIZE: usize = 24;

/// Version of this layout, kept in the low byte of `pd_pagesize_version`.
/// Postgres's `PG_PAGE_LAYOUT_VERSION`, which has been 4 since Postgres 8.3.
pub const PAGE_LAYOUT_VERSION: u16 = 4;

/// Where `pd_checksum` sits in the page.
pub(crate) const PD_CHECKSUM: Range<usize> = 8..10;

/// Every flag Postgres defines: has free line pointers, page full, all visible.
const PD_VALID_FLAG_BITS: u16 = 0x0007;

/// Postgres's `MAXIMUM_ALIGNOF`: This is the maximum alignment a basic C type may require.
const MAXIMUM_ALIGNOF: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Log Sequence Number: Position in the WAL just past the last record that changed the page.
    pub lsn: u64,
    pub checksum: u16,
    pub flags: u16,
    /// Offset of the start of free space, just past the last line pointer.
    pub lower: u16,
    /// Offset of the end of free space, where tuple data begins.
    pub upper: u16,
    /// Offset of the start of the special space, which the access method reserves.
    pub special: u16,
    /// Page size in the high bits, layout version in the low byte.
    pub pagesize_version: u16,
    /// Oldest transaction whose deletions on this page could be pruned, or 0.
    pub prune_xid: u32,
}

impl Page {
    pub fn header(&self) -> Header {
        // Postgres stores the LSN as two 32-bit halves, high half first.
        let lsn = (u64::from(read_u32(self, 0)) << 32) | u64::from(read_u32(self, 4));
        Header {
            lsn,
            checksum: read_u16(self, 8),
            flags: read_u16(self, 10),
            lower: read_u16(self, 12),
            upper: read_u16(self, 14),
            special: read_u16(self, 16),
            pagesize_version: read_u16(self, 18),
            prune_xid: read_u32(self, 20),
        }
    }

    /// Turns self into an empty page with at least `special_size` bytes of special space
    /// at the end. Postgres's `PageInit`.
    pub fn init(page: &mut Page, special_size: usize) {
        let special_size = special_size.next_multiple_of(MAXIMUM_ALIGNOF);
        assert!(
            HEADER_SIZE + special_size <= BLCKSZ,
            "special space too large"
        );
        let special = (BLCKSZ - special_size) as u16;
        page.fill(0);
        page.set_header(Header {
            lsn: 0,
            checksum: 0,
            flags: 0,
            lower: HEADER_SIZE as u16,
            upper: special,
            special,
            pagesize_version: BLCKSZ as u16 | PAGE_LAYOUT_VERSION,
            prune_xid: 0,
        });
    }

    pub fn is_uninitialized(&self) -> bool {
        self.header().upper == 0
    }

    pub fn set_header(&mut self, header: Header) {
        write_u32(self, 0, (header.lsn >> 32) as u32);
        write_u32(self, 4, header.lsn as u32);
        write_u16(self, 8, header.checksum);
        write_u16(self, 10, header.flags);
        write_u16(self, 12, header.lower);
        write_u16(self, 14, header.upper);
        write_u16(self, 16, header.special);
        write_u16(self, 18, header.pagesize_version);
        write_u32(self, 20, header.prune_xid);
    }

    /// Set the checksum for self at `block`, and locks self into a validated state.
    pub fn try_set_checksum_for_block(
        mut self,
        block: BlockNumber,
    ) -> Result<ValidatedPage<'static>, PageError> {
        if self.is_uninitialized() {
            return if self.iter().all(|&byte| byte == 0) {
                Ok(ValidatedPage {
                    page: Cow::Owned(self),
                    block,
                })
            } else {
                Err(PageError::InvalidHeader { block })
            };
        }

        if !self.header().is_sane() {
            return Err(PageError::InvalidHeader { block });
        }

        let checksum = checksum_page(&self, block);
        self[PD_CHECKSUM].copy_from_slice(&checksum.to_le_bytes());
        Ok(ValidatedPage {
            page: Cow::Owned(self),
            block,
        })
    }

    /// Try to validate self for `block`. Returns an error if the checksum doesn't match or the
    /// header is invalid.
    pub fn try_validated_for_block<'a>(
        &'a self,
        block: BlockNumber,
    ) -> Result<ValidatedPage<'a>, PageError> {
        if self.is_uninitialized() {
            return if self.iter().all(|&byte| byte == 0) {
                Ok(ValidatedPage {
                    page: Cow::Borrowed(self),
                    block,
                })
            } else {
                Err(PageError::InvalidHeader { block })
            };
        }
        let header = self.header();
        let computed = checksum_page(self, block);
        if header.checksum != computed {
            return Err(PageError::ChecksumMismatch {
                block,
                stored: header.checksum,
                computed,
            });
        }
        if !header.is_sane() {
            return Err(PageError::InvalidHeader { block });
        }
        Ok(ValidatedPage {
            page: Cow::Borrowed(self),
            block,
        })
    }
}

impl Header {
    /// The checks Postgres runs before letting a page into the buffer pool.
    /// They don't prove the header right, only plausible.
    fn is_sane(&self) -> bool {
        self.flags & !PD_VALID_FLAG_BITS == 0
            && self.lower <= self.upper
            && self.upper <= self.special
            && usize::from(self.special) <= BLCKSZ
            && usize::from(self.special) % MAXIMUM_ALIGNOF == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageError {
    ChecksumMismatch {
        block: BlockNumber,
        stored: u16,
        computed: u16,
    },
    InvalidHeader {
        block: BlockNumber,
    },
}

impl fmt::Display for PageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PageError::ChecksumMismatch {
                block,
                stored,
                computed,
            } => write!(
                f,
                "page verification failed in block {block}: \
                 calculated checksum {computed} but expected {stored}"
            ),
            PageError::InvalidHeader { block } => write!(f, "invalid page header in block {block}"),
        }
    }
}

impl std::error::Error for PageError {}

fn read_u16(page: &Page, offset: usize) -> u16 {
    u16::from_le_bytes([page[offset], page[offset + 1]])
}

fn read_u32(page: &Page, offset: usize) -> u32 {
    u32::from_le_bytes(page[offset..offset + 4].try_into().unwrap())
}

fn write_u16(page: &mut Page, offset: usize, value: u16) {
    page[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(page: &mut Page, offset: usize, value: u32) {
    page[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An initialized page with some arbitrary bytes in its free space.
    fn sample_page() -> Page {
        let mut page = Page { bytes: [0; BLCKSZ] };
        Page::init(&mut page, 16);
        for (i, byte) in page[HEADER_SIZE..BLCKSZ - 16].iter_mut().enumerate() {
            *byte = (i * 31 % 251) as u8;
        }
        page
    }

    /// `page` with its checksum set for `block`.
    fn sealed(page: Page, block: BlockNumber) -> Page {
        let validated = page.try_set_checksum_for_block(block).unwrap();
        validated.page().clone()
    }

    #[test]
    fn init_matches_page_init() {
        let mut page = Page {
            bytes: [0xAB; BLCKSZ],
        };
        Page::init(&mut page, 12);
        let header = page.header();
        assert_eq!(header.lower, 24);
        assert_eq!(header.upper, 8192 - 16);
        assert_eq!(header.special, 8192 - 16);
        assert_eq!(header.pagesize_version, 8196);
        assert!(page[HEADER_SIZE..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn header_round_trips() {
        let header = Header {
            lsn: 0x0000_0001_0A2B_3C4D,
            checksum: 0x1234,
            flags: 0x0005,
            lower: 40,
            upper: 8000,
            special: 8176,
            pagesize_version: 8196,
            prune_xid: 742,
        };
        let mut page = Page { bytes: [0; BLCKSZ] };
        page.set_header(header);
        assert_eq!(page.header(), header);
        // High half of the LSN first, each half little-endian.
        assert_eq!(page[..8], [0x01, 0, 0, 0, 0x4D, 0x3C, 0x2B, 0x0A]);
    }

    #[test]
    fn all_zero_page_verifies() {
        // `extend` adds pages of zeros, which have no checksum.
        let page = Page { bytes: [0; BLCKSZ] };
        assert!(page.try_validated_for_block(BlockNumber(7)).is_ok());
    }

    #[test]
    fn set_checksum_leaves_a_new_page_all_zeros() {
        // A checksum would make the page no longer all zeros while `upper`
        // is still 0, and `verify` would reject it as an invalid header.
        let page = sealed(Page { bytes: [0; BLCKSZ] }, BlockNumber(7));
        assert_eq!(page.bytes, [0; BLCKSZ]);
    }

    #[test]
    fn checksummed_page_verifies() {
        let page = sealed(sample_page(), BlockNumber(3));
        assert_ne!(page.header().checksum, 0);
        assert!(page.try_validated_for_block(BlockNumber(3)).is_ok());
    }

    #[test]
    fn every_flipped_byte_is_caught() {
        let page = sealed(sample_page(), BlockNumber(0));
        for i in 0..BLCKSZ {
            let mut corrupt = page.clone();
            corrupt[i] ^= 0xFF;
            assert!(
                corrupt.try_validated_for_block(BlockNumber(0)).is_err(),
                "flipping byte {i} went unnoticed"
            );
        }
    }

    #[test]
    fn page_at_the_wrong_block_is_caught() {
        let page = sealed(sample_page(), BlockNumber(5));
        for block in (0..1000u32).filter(|&block| block != 5) {
            assert!(matches!(
                page.try_validated_for_block(BlockNumber(block)),
                Err(PageError::ChecksumMismatch { .. })
            ));
        }
    }

    #[test]
    fn insane_header_is_caught_even_with_a_good_checksum() {
        let mut page = sample_page();
        let mut header = page.header();
        header.lower = header.upper + 8;
        page.set_header(header);
        // Sealing refuses such a page, so write its checksum directly.
        let checksum = checksum_page(&page, BlockNumber(0));
        page[PD_CHECKSUM].copy_from_slice(&checksum.to_le_bytes());
        assert!(matches!(
            page.try_validated_for_block(BlockNumber(0)),
            Err(PageError::InvalidHeader {
                block: BlockNumber(0)
            })
        ));
    }

    #[test]
    fn sealing_an_insane_header_fails() {
        let mut page = sample_page();
        let mut header = page.header();
        header.lower = header.upper + 8;
        page.set_header(header);
        assert_eq!(
            page.try_set_checksum_for_block(BlockNumber(0)).err(),
            Some(PageError::InvalidHeader {
                block: BlockNumber(0)
            })
        );
    }

    #[test]
    fn zero_upper_with_data_is_not_a_new_page() {
        let mut page = Page { bytes: [0; BLCKSZ] };
        page[100] = 1;
        assert!(matches!(
            page.try_validated_for_block(BlockNumber(0)),
            Err(PageError::InvalidHeader {
                block: BlockNumber(0)
            })
        ));
    }
}
