//! Handles line pointers and TID that refer to tuples through them.
//!
//! Tuples' address are expressed in offset by their line pointer's position in
//! the array rather than by their offset in the page, so a page can move its
//! tuples around (to compact free space) without changing their addresses.
//!
//! See [itemid.h](https://github.com/postgres/postgres/blob/master/src/include/storage/itemid.h),
//! [itemptr.h](https://github.com/postgres/postgres/blob/master/src/include/storage/itemptr.h)
//! and [off.h](https://github.com/postgres/postgres/blob/master/src/include/storage/off.h).

use super::{
    BlockNumber,
    page::{HEADER_SIZE, InvalidHeader, LocationIndex, Page},
};

use std::{fmt, num::NonZeroU16};

/// Identifier of a tuple: a block, and the position a line pointer in the block. Postgres's
/// `ItemPointerData`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Tid {
    pub block: BlockNumber,
    pub offset: OffsetNumber,
}

/// Size of a TID on disk. Postgres's `sizeof(ItemPointerData)`.
pub const TID_SIZE: usize = 6;

impl Tid {
    /// Reads a TID as stored in a tuple. The block number is two 16-bit
    /// halves, high half first, so that the struct needs only 2-byte
    /// alignment: Postgres's `BlockIdData`.
    pub fn from_bytes(bytes: &[u8; TID_SIZE]) -> Option<Self> {
        let half = |i: usize| u32::from(u16::from_le_bytes([bytes[i], bytes[i + 1]]));
        Some(Tid {
            block: BlockNumber((half(0) << 16) | half(2)),
            offset: OffsetNumber(NonZeroU16::new(u16::from_le_bytes([bytes[4], bytes[5]]))?),
        })
    }
}

/// `(block,offset)`, as Postgres prints a `ctid`.
impl fmt::Display for Tid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({},{})", self.block.0, self.offset.0)
    }
}

/// Position of a line pointer in a page's array, counting from 1.
///
/// This minus one is the index in an array of line pointers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OffsetNumber(pub NonZeroU16);

/// Size of a line pointer. Postgres's `sizeof(ItemIdData)`.
pub const LINE_POINTER_SIZE: usize = 4;

/// A line pointer, Postgres's `ItemIdData`.
///
/// This is what TID refer to. A LinePointer either:
///  * Points to a tuple
///  * Points to another line pointer on the same page
///  * Points to nothing, because nothing reference the corresponding TID anymore
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinePointer {
    /// Nothing references the corresponding TID anymore.
    Unused,
    /// This TID refers to a used tuple.
    Normal { offset: LocationIndex, len: u16 },
    /// Points at another line pointer on the same page.
    /// This TID referrs to a slot that used to hold a dead tuple, whose bytes have now
    /// been removed from the page. The LinePointer now holds the offset of the `ctid` of the tuple
    /// that was removed. Postgres's `LP_REDIRECT`.
    /// See [Heap-Only Tuples](https://www.postgresql.org/docs/current/storage-hot.html).
    Redirect(Option<OffsetNumber>),
    /// This TID refers to a slot that is not used but whose bytes are still present in the page.
    Dead { offset: LocationIndex, len: u16 },
}

impl From<u32> for LinePointer {
    fn from(raw: u32) -> Self {
        // Postgres's `ItemIdData`: packs the following into 32 bits:
        // - offset into the page (15 bits)
        // - a state (2 bits)
        // - a length (15 bits)
        let offset = (raw & 0b_0111_1111_1111_1111) as u16;
        let len = (raw >> 17) as u16;
        match (raw >> 15) & 0b_11 {
            0b_00 => LinePointer::Unused,
            0b_01 => LinePointer::Normal {
                offset: LocationIndex(offset),
                len,
            },
            0b_10 => LinePointer::Redirect(NonZeroU16::new(offset).map(OffsetNumber)),
            0b_11 => LinePointer::Dead {
                offset: LocationIndex(offset),
                len,
            },
            _ => unreachable!(),
        }
    }
}

impl Page {
    /// The line pointer array, between the header and `pd_lower`. Empty if
    /// `pd_lower` doesn't point past the header, as on a new, all-zero page.
    fn line_pointers(&self) -> Result<&[[u8; LINE_POINTER_SIZE]], InvalidHeader> {
        if self.is_uninitialized() {
            return Ok(&[].as_chunks().0);
        }
        let header = self.header()?;
        Ok(self
            .get(HEADER_SIZE..usize::from(header.lower.0))
            .ok_or(
                // self.header() checked that pd_lower was <= BLCKSZ, so this is the only possible
                // explanation
                InvalidHeader::PdLowerTooSmall {
                    lower: header.lower,
                },
            )?
            .as_chunks()
            .0)
    }

    pub fn max_offset_number(&self) -> Result<Option<OffsetNumber>, InvalidHeader> {
        Ok(NonZeroU16::new(self.line_pointers()?.len() as u16).map(OffsetNumber))
    }

    pub fn line_pointer(&self, offset: OffsetNumber) -> Result<Option<LinePointer>, InvalidHeader> {
        let index = usize::from(offset.0.get()) - 1;
        let Some(raw) = self.line_pointers()?.get(index) else {
            return Ok(None);
        };
        Ok(Some(LinePointer::from(u32::from_le_bytes(*raw))))
    }
}
