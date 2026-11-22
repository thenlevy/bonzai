//! Handles line pointers, and the TIDs that refer to tuples through them.
//!
//! A tuple's address is expressed by its line pointer's position in the page's
//! array rather than by its byte offset in the page, so a page can move its
//! tuples around (to compact free space) without changing their addresses.
//!
//! See [itemid.h](https://github.com/postgres/postgres/blob/master/src/include/storage/itemid.h),
//! [itemptr.h](https://github.com/postgres/postgres/blob/master/src/include/storage/itemptr.h)
//! and [off.h](https://github.com/postgres/postgres/blob/master/src/include/storage/off.h).

use super::{
    BlockNumber,
    page::{HEADER_SIZE_BYTES, InvalidHeader, Page, PageOffset, PageWithValidatedHeader},
};

use std::{fmt, num::NonZeroU16};

/// The raw bytes a line pointer points to. Postgres's `Item`.
pub struct Item<'a>(pub &'a [u8]);

/// Identifier of a tuple: a block, and the position of a line pointer in the block. Postgres's
/// `ItemPointerData`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Tid {
    pub block: BlockNumber,
    pub line_pointer_number: LinePointerNumber,
}

pub const TID_SIZE_BYTES: usize = 6;

impl Tid {
    /// Reads a TID as stored in a tuple. The block number is two 16-bit
    /// halves, high half first, so that the struct needs only 2-byte
    /// alignment: Postgres's `BlockIdData`.
    pub fn from_bytes(bytes: &[u8; TID_SIZE_BYTES]) -> Option<Self> {
        let half = |i: usize| u32::from(u16::from_le_bytes([bytes[i], bytes[i + 1]]));
        Some(Tid {
            block: BlockNumber((half(0) << 16) | half(2)),
            line_pointer_number: LinePointerNumber(NonZeroU16::new(u16::from_le_bytes([
                bytes[4], bytes[5],
            ]))?),
        })
    }
}

/// `(block,line pointer number)`, as Postgres prints a `ctid`.
impl fmt::Display for Tid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({},{})", self.block.0, self.line_pointer_number.0)
    }
}

/// Position of a line pointer in a page's array, counting from 1.
///
/// This minus one is the index in the page's array of line pointers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LinePointerNumber(pub NonZeroU16);

/// Size of a line pointer. Postgres's `sizeof(ItemIdData)`.
pub const LINE_POINTER_SIZE_BYTES: usize = 4;

/// A line pointer, Postgres's `ItemIdData`.
///
/// This is what a TID refers to. A line pointer either:
///  * points to a tuple,
///  * points to another line pointer on the same page,
///  * or points to nothing, because its tuple is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinePointer {
    /// Points to nothing. Nothing references the corresponding TID anymore, so the next insert
    /// on the page can reuse it. Postgres's `LP_UNUSED`.
    Unused,
    /// This TID refers to a tuple: `len` bytes starting at byte `offset` of the page. The tuple
    /// can be any version of its row. Postgres's `LP_NORMAL`.
    Normal { offset: PageOffset, len: u16 },
    /// Points to another line pointer on the same page.
    /// This TID refers to a slot that used to hold a dead tuple, whose bytes have now
    /// been removed from the page, while its row lives on in a newer version on the same page.
    /// The line pointer holds the line pointer number of the first version that survived, or
    /// `None` if that number is 0, which Postgres never writes: the page is corrupt.
    /// Postgres's `LP_REDIRECT`.
    /// See [Heap-Only Tuples](https://www.postgresql.org/docs/current/storage-hot.html).
    Redirect(Option<LinePointerNumber>),
    /// Points to nothing. This TID refers to a slot whose tuple is dead to every transaction,
    /// but indexes may still hold the TID, so the slot cannot be reused yet. The tuple's bytes
    /// may still be in the page, as on B-tree pages, or already removed with a `len` of 0, as
    /// on heap pages after pruning. Postgres's `LP_DEAD`.
    Dead { offset: PageOffset, len: u16 },
}

impl From<u32> for LinePointer {
    fn from(raw: u32) -> Self {
        // Postgres's `ItemIdData` packs the following into 32 bits, from the lowest bits up:
        // - `lp_off` (15 bits): a byte offset into the page, except in a redirect, where it is a
        //   line pointer number
        // - a state (2 bits)
        // - a length (15 bits)
        let lp_off = (raw & 0b_0111_1111_1111_1111) as u16;
        let len = (raw >> 17) as u16;
        match (raw >> 15) & 0b_11 {
            0b_00 => LinePointer::Unused,
            0b_01 => LinePointer::Normal {
                offset: PageOffset(lp_off),
                len,
            },
            0b_10 => LinePointer::Redirect(NonZeroU16::new(lp_off).map(LinePointerNumber)),
            0b_11 => LinePointer::Dead {
                offset: PageOffset(lp_off),
                len,
            },
            _ => unreachable!(),
        }
    }
}

#[derive(Debug)]
pub struct InvalidItemError {
    pub offset: PageOffset,
    pub len: u16,
    pub page_pd_upper: PageOffset,
    pub page_pd_special: PageOffset,
}

impl Page {
    /// The line pointer array, between the header and `pd_lower`. Empty on a
    /// new, all-zero page.
    fn line_pointers(&self) -> Result<&[[u8; LINE_POINTER_SIZE_BYTES]], InvalidHeader> {
        if self.is_uninitialized() {
            return Ok(&[].as_chunks().0);
        }
        let header = self.header()?;
        Ok(self
            .get(HEADER_SIZE_BYTES..usize::from(header.lower.0))
            .ok_or(
                // self.header() checked that pd_lower was <= BLCKSZ_BYTES, so this is the only
                // possible explanation
                InvalidHeader::PdLowerTooSmall {
                    lower: header.lower,
                },
            )?
            .as_chunks()
            .0)
    }

    /// The number of the page's last line pointer, or `None` if the page has no line pointer.
    /// Postgres's `PageGetMaxOffsetNumber`.
    pub fn max_line_pointer_number(&self) -> Result<Option<LinePointerNumber>, InvalidHeader> {
        Ok(NonZeroU16::new(self.line_pointers()?.len() as u16).map(LinePointerNumber))
    }

    pub fn line_pointer(
        &self,
        line_pointer_number: LinePointerNumber,
    ) -> Result<Option<LinePointer>, InvalidHeader> {
        let index = usize::from(line_pointer_number.0.get()) - 1;
        let Some(raw) = self.line_pointers()?.get(index) else {
            return Ok(None);
        };
        Ok(Some(LinePointer::from(u32::from_le_bytes(*raw))))
    }
}

impl PageWithValidatedHeader<'_> {
    pub fn item(&self, offset: PageOffset, len: u16) -> Result<Item<'_>, InvalidItemError> {
        let header = self.header();
        // Tuples live between `pd_upper` and `pd_special`.
        let end = usize::from(offset.0) + usize::from(len);
        if offset.0 < header.upper.0 || end > usize::from(header.special.0) {
            return Err(InvalidItemError {
                offset,
                len,
                page_pd_upper: header.upper,
                page_pd_special: header.special,
            });
        }
        Ok(Item(self.page.get(usize::from(offset.0)..end).ok_or(
            InvalidItemError {
                offset,
                len,
                page_pd_upper: header.upper,
                page_pd_special: header.special,
            },
        )?))
    }
}
