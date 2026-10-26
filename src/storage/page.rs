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

use {
    super::{BLCKSZ, BlockNumber, checksum::checksum_page},
    crate::{TransactionId, XLogRecPtr},
};

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

/// Postgres's `MAXIMUM_ALIGNOF`: This is the maximum alignment a basic C type may require.
const MAXIMUM_ALIGNOF: usize = 8;

/// Offset of a byte within a page. Postgres's `LocationIndex`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocationIndex(pub u16);

impl From<LocationIndex> for usize {
    fn from(location: LocationIndex) -> Self {
        usize::from(location.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Log Sequence Number: Position in the WAL just past the last record that changed the page,
    /// if any has.
    /// See [WAL Internals](https://www.postgresql.org/docs/current/wal-internals.html).
    pub lsn: Option<XLogRecPtr>,
    pub checksum: u16,
    pub flags: PageFlags,
    /// Offset of the start of free space, just past the last line pointer.
    pub lower: LocationIndex,
    /// Offset of the end of free space, where tuple data begins.
    pub upper: LocationIndex,
    /// Offset of the start of the special space, which the access method reserves.
    /// See [Database Page Layout](https://www.postgresql.org/docs/current/storage-page-layout.html).
    pub special: LocationIndex,
    /// Page size in the high bits, layout version in the low byte.
    pub pagesize_version: u16,
    /// Oldest transaction whose deletions on this page could be pruned, if any.
    /// See [Heap-Only Tuples (HOT)](https://www.postgresql.org/docs/current/storage-hot.html).
    pub prune_xid: Option<TransactionId>,
}

impl Page {
    /// The page's header, parsed and checked by [`Header::try_from_raw`].
    pub fn header(&self) -> Result<Header, InvalidHeader> {
        Header::try_from_raw(self.header_raw())
    }

    /// The bytes of the page's header.
    pub fn header_raw(&self) -> &[u8; HEADER_SIZE] {
        self.bytes.first_chunk().unwrap()
    }

    /// Turns self into an empty page with at least `special_size` bytes of special space
    /// at the end. Postgres's `PageInit`.
    pub fn init(page: &mut Page, special_size: usize) {
        let special_size = special_size.next_multiple_of(MAXIMUM_ALIGNOF);
        assert!(
            HEADER_SIZE + special_size <= BLCKSZ,
            "special space too large"
        );
        let special = LocationIndex((BLCKSZ - special_size) as u16);
        page.fill(0);
        page.set_header(Header {
            lsn: None,
            checksum: 0,
            flags: PageFlags::default(),
            lower: LocationIndex(HEADER_SIZE as u16),
            upper: special,
            special,
            pagesize_version: BLCKSZ as u16 | PAGE_LAYOUT_VERSION,
            prune_xid: None,
        });
    }

    pub fn is_uninitialized(&self) -> bool {
        // `pd_upper`, read on its own: the rest of the header may not parse.
        read_u16(self, 14) == 0
    }

    pub fn set_header(&mut self, header: Header) {
        let lsn = header.lsn.map_or(0, u64::from);
        write_u32(self, 0, (lsn >> 32) as u32);
        write_u32(self, 4, lsn as u32);
        write_u16(self, 8, header.checksum);
        write_u16(self, 10, header.flags.into());
        write_u16(self, 12, header.lower.0);
        write_u16(self, 14, header.upper.0);
        write_u16(self, 16, header.special.0);
        write_u16(self, 18, header.pagesize_version);
        write_u32(self, 20, header.prune_xid.map_or(0, u32::from));
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

        if self.header().is_err() {
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
        // Read on its own, so that a checksum mismatch is reported even when
        // `header()` rejects the header.
        let stored = read_u16(self, PD_CHECKSUM.start);
        let computed = checksum_page(self, block);
        if stored != computed {
            return Err(PageError::ChecksumMismatch {
                block,
                stored,
                computed,
            });
        }
        if self.header().is_err() {
            return Err(PageError::InvalidHeader { block });
        }
        Ok(ValidatedPage {
            page: Cow::Borrowed(self),
            block,
        })
    }
}

impl Header {
    /// Parses a header, with the checks Postgres runs before letting a page
    /// into the buffer pool. They don't prove the header right, only plausible.
    pub fn try_from_raw(raw: &[u8; HEADER_SIZE]) -> Result<Self, InvalidHeader> {
        // Postgres stores the LSN as two 32-bit halves, high half first.
        let lsn = (u64::from(read_u32(raw, 0)) << 32) | u64::from(read_u32(raw, 4));
        let header = Header {
            lsn: XLogRecPtr::from_raw(lsn),
            checksum: read_u16(raw, 8),
            flags: PageFlags::try_from(read_u16(raw, 10))?,
            lower: LocationIndex(read_u16(raw, 12)),
            upper: LocationIndex(read_u16(raw, 14)),
            special: LocationIndex(read_u16(raw, 16)),
            pagesize_version: read_u16(raw, 18),
            prune_xid: TransactionId::from_raw(read_u32(raw, 20)),
        };
        let Header {
            lower,
            upper,
            special,
            ..
        } = header;
        if !(lower <= upper
            && upper <= special
            && usize::from(special) <= BLCKSZ
            && usize::from(special) % MAXIMUM_ALIGNOF == 0)
        {
            return Err(InvalidHeader::Offsets {
                lower,
                upper,
                special,
            });
        }
        Ok(header)
    }
}

/// Why a header fails the checks Postgres runs before letting a page into the
/// buffer pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidHeader {
    /// `pd_flags` has a bit set that Postgres doesn't define.
    Flags(u16),
    /// `pd_lower`, `pd_upper` and `pd_special` are out of order or off the
    /// page, or `pd_special` isn't aligned.
    Offsets {
        lower: LocationIndex,
        upper: LocationIndex,
        special: LocationIndex,
    },
}

impl fmt::Display for InvalidHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InvalidHeader::Flags(bits) => write!(f, "invalid page flags {bits:#06x}"),
            InvalidHeader::Offsets {
                lower,
                upper,
                special,
            } => write!(
                f,
                "invalid page offsets: lower {}, upper {}, special {}",
                lower.0, upper.0, special.0
            ),
        }
    }
}

impl std::error::Error for InvalidHeader {}

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

/// The bits of `pd_flags`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PageFlags {
    /// Some line pointers before `pd_lower` are unused and can be reused. A
    /// hint: changes to it aren't WAL-logged. Postgres's `PD_HAS_FREE_LINES`.
    pub has_free_lines: bool,
    /// An UPDATE found no room on the page for its new row version, so pruning
    /// the page may help. A hint too. Postgres's `PD_PAGE_FULL`.
    pub page_full: bool,
    /// Every row on the page is visible to every transaction, current and
    /// future. Set by VACUUM, along with the page's bit in the visibility map.
    /// Postgres's `PD_ALL_VISIBLE`.
    pub all_visible: bool,
}

const PD_HAS_FREE_LINES: u16 = 0x0001;
const PD_PAGE_FULL: u16 = 0x0002;
const PD_ALL_VISIBLE: u16 = 0x0004;
/// Every flag Postgres defines.
const PD_VALID_FLAG_BITS: u16 = PD_HAS_FREE_LINES | PD_PAGE_FULL | PD_ALL_VISIBLE;

/// Fails if a bit Postgres doesn't define is set.
impl TryFrom<u16> for PageFlags {
    type Error = InvalidHeader;

    fn try_from(bits: u16) -> Result<Self, Self::Error> {
        if bits & !PD_VALID_FLAG_BITS != 0 {
            return Err(InvalidHeader::Flags(bits));
        }
        Ok(PageFlags {
            has_free_lines: bits & PD_HAS_FREE_LINES != 0,
            page_full: bits & PD_PAGE_FULL != 0,
            all_visible: bits & PD_ALL_VISIBLE != 0,
        })
    }
}

impl From<PageFlags> for u16 {
    fn from(flags: PageFlags) -> Self {
        let mut bits = 0;
        if flags.has_free_lines {
            bits |= PD_HAS_FREE_LINES;
        }
        if flags.page_full {
            bits |= PD_PAGE_FULL;
        }
        if flags.all_visible {
            bits |= PD_ALL_VISIBLE;
        }
        bits
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
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
        let header = page.header().unwrap();
        assert_eq!(header.lower, LocationIndex(24));
        assert_eq!(header.upper, LocationIndex(8192 - 16));
        assert_eq!(header.special, LocationIndex(8192 - 16));
        assert_eq!(header.pagesize_version, 8196);
        assert_eq!(header.lsn, None);
        assert_eq!(header.prune_xid, None);
        assert!(page[HEADER_SIZE..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn header_round_trips() {
        let header = Header {
            lsn: XLogRecPtr::from_raw(0x0000_0001_0A2B_3C4D),
            checksum: 0x1234,
            flags: PageFlags {
                has_free_lines: true,
                page_full: false,
                all_visible: true,
            },
            lower: LocationIndex(40),
            upper: LocationIndex(8000),
            special: LocationIndex(8176),
            pagesize_version: 8196,
            prune_xid: TransactionId::from_raw(742),
        };
        let mut page = Page { bytes: [0; BLCKSZ] };
        page.set_header(header);
        assert_eq!(page.header(), Ok(header));
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
        assert_ne!(page.header().unwrap().checksum, 0);
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
        let mut header = page.header().unwrap();
        header.lower = LocationIndex(header.upper.0 + 8);
        page.set_header(header);
        assert!(matches!(page.header(), Err(InvalidHeader::Offsets { .. })));
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
    fn unknown_flag_is_caught_even_with_a_good_checksum() {
        let mut page = sample_page();
        // No `PageFlags` can hold this bit, so write `pd_flags` directly.
        write_u16(&mut page, 10, 0x0008);
        assert_eq!(page.header(), Err(InvalidHeader::Flags(0x0008)));
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
        let mut header = page.header().unwrap();
        header.lower = LocationIndex(header.upper.0 + 8);
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
