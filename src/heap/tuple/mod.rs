//! A heap tuple: a header, an optional null bitmap, then the row's columns.
//!
//! ```text
//! +--------------------------+-------------+---------+----------------------+
//! | fixed header (23 bytes)  | null bitmap | padding | columns, each aligned |
//! +--------------------------+-------------+---------+----------------------+
//! 0                          23                     t_hoff
//! ```
//!
//! See [htup_details.h](https://github.com/postgres/postgres/blob/master/src/include/access/htup_details.h),
//! [varatt.h](https://github.com/postgres/postgres/blob/master/src/include/varatt.h), and the
//! [Table Row Layout](https://www.postgresql.org/docs/current/storage-page-layout.html#STORAGE-TUPLE-LAYOUT)
//! section of the Postgres docs.

pub mod deform;

use crate::{
    TransactionId,
    storage::{
        item::{Item, Tid},
        page::MAXIMUM_ALIGNOF_BYTES,
    },
};

pub struct Tuple<'a> {
    header: TupleHeader,

    null_bitmap: Option<&'a [u8]>,

    columns_data: &'a [u8],
}

impl Tuple<'_> {
    pub fn header(&self) -> &TupleHeader {
        &self.header
    }
}

/// Postgres's `HeapTupleHeaderData`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TupleHeader {
    /// The transaction that inserted the tuple.
    pub xmin: Option<TransactionId>,
    /// The transaction that deleted or locked the tuple, if any.
    pub xmax: Option<TransactionId>,
    /// Which command of its transaction inserted or deleted the tuple.
    /// Postgres's `t_cid`, which shares its 4 bytes with `t_xvac`, set only by
    /// the old VACUUM FULL of Postgres 8.4 and before.
    pub cid: u32,
    /// The tuple's own TID, or its newer version's once an UPDATE wrote one.
    /// `None` if its line pointer number is 0, which Postgres never writes:
    /// the page is corrupt.
    pub ctid: Option<Tid>,
    /// = 0b_OHK0_0nnn_nnnn_nnnn
    ///
    /// - `O` [`Self::heap_only`]
    /// - `H` [`Self::hot_updated`]
    /// - `K` [`Self::keys_updated`]
    /// - `0` unused
    /// - `n` [`Self::natts`]
    pub infomask2: u16,
    /// = 0b_IOUM_hhhh_LECK_oevn
    ///
    /// - `I` [`Self::moved_in`]
    /// - `O` [`Self::moved_off`]
    /// - `U` [`Self::updated`]
    /// - `M` [`Self::xmax_is_multi`]
    /// - `h` hint bits, left to right [`Self::xmax_invalid`], [`Self::xmax_committed`],
    ///   [`Self::xmin_invalid`], [`Self::xmin_committed`]
    /// - `L` [`Self::xmax_lock_only`]
    /// - `E` [`Self::xmax_excl_lock`]
    /// - `C` [`Self::combo_cid`]
    /// - `K` [`Self::xmax_keyshr_lock`]
    /// - `o` [`Self::has_oid`]
    /// - `e` [`Self::has_external`]
    /// - `v` [`Self::has_varwidth`]
    /// - `n` [`Self::has_null`]
    pub infomask: u16,
    /// Size of the tuple header in bytes: its fixed part, the null bitmap, and
    /// the padding that aligns the columns to [`MAXIMUM_ALIGNOF_BYTES`]. The
    /// columns start that many bytes into the tuple. Postgres's `t_hoff`.
    pub header_size: u8,
}

impl<'a> Item<'a> {
    pub fn try_into_heap_tuple(self) -> Result<Tuple<'a>, TupleError> {
        let header_data: &[u8; FIXED_HEADER_SIZE_BYTES] =
            self.0.first_chunk().ok_or(TupleError::Truncated)?;

        macro_rules! u16_at {
            ($i:expr) => {
                u16::from_le_bytes([header_data[$i], header_data[$i + 1]])
            };
        }
        macro_rules! u32_at {
            ($i:expr) => {
                u32::from_le_bytes([
                    header_data[$i],
                    header_data[$i + 1],
                    header_data[$i + 2],
                    header_data[$i + 3],
                ])
            };
        }

        let header = TupleHeader {
            xmin: TransactionId::from_raw(u32_at!(0)),
            xmax: TransactionId::from_raw(u32_at!(4)),
            cid: u32_at!(8),
            ctid: Tid::from_bytes(
                header_data[12..]
                    .first_chunk()
                    .ok_or(TupleError::Truncated)?,
            ),
            infomask2: u16_at!(18),
            infomask: u16_at!(20),
            header_size: header_data[22],
        };
        let bitmap_len = if header.has_null() {
            usize::from(header.natts()).div_ceil(8)
        } else {
            0
        };
        let header_size = usize::from(header.header_size);
        if header_size < FIXED_HEADER_SIZE_BYTES + bitmap_len
            || header_size % MAXIMUM_ALIGNOF_BYTES != 0
        {
            return Err(TupleError::BadHeaderSize {
                header_size: header.header_size,
            });
        }
        let columns_data = self.0.get(header_size..).ok_or(TupleError::Truncated)?;
        let bitmap = &self.0[FIXED_HEADER_SIZE_BYTES..FIXED_HEADER_SIZE_BYTES + bitmap_len];
        Ok(Tuple {
            header,
            null_bitmap: (bitmap_len > 0).then_some(bitmap),
            columns_data,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Why a tuple cannot be parsed as a row.
pub enum TupleError {
    /// The tuple is shorter than its header or one of its lengths says.
    Truncated,
    /// `t_hoff` leaves no room for the fixed part of the header and the null
    /// bitmap, or isn't aligned.
    BadHeaderSize {
        header_size: u8,
    },
    /// The tuple doesn't have the expected number of columns.
    NumberOfAttributes {
        natts: u16,
        expected: u16,
    },
    /// NULL in a column that can't be NULL.
    UnexpectedNull {
        attnum: u16,
    },
    InvalidUtf8,
    /// The tuple may be valid, but bonsai can't read it yet.
    Unimplemented(UnimplementedTuple),
}

/// Reasons why a valid tuple cannot be converted into a row by bonsai.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnimplementedTuple {
    /// A value moved out to TOAST.
    Toasted,
    /// A compressed value.
    Compressed,
    /// The tuple is currently unsupported by bonsai.
    Unsupported { attnum: u16 },
    /// Whether the tuple is visible cannot be inferred from the hint bits alone.
    UnsupportedVisibility { infomask: u16 },
}

/// Size of the fixed part of a tuple header, which the null bitmap follows. Postgres's
/// `SizeofHeapTupleHeader`.
pub(crate) const FIXED_HEADER_SIZE_BYTES: usize = 23;

const HEAP_ONLY_TUPLE: u16 = 0x8000;
const HEAP_HOT_UPDATED: u16 = 0x4000;
const HEAP_KEYS_UPDATED: u16 = 0x2000;
const HEAP_NATTS_MASK: u16 = 0x07FF;

const HEAP_MOVED_IN: u16 = 0x8000;
const HEAP_MOVED_OFF: u16 = 0x4000;
const HEAP_UPDATED: u16 = 0x2000;
const HEAP_XMAX_IS_MULTI: u16 = 0x1000;
pub(crate) const HEAP_XMAX_INVALID: u16 = 0x0800;
const HEAP_XMAX_COMMITTED: u16 = 0x0400;
pub(crate) const HEAP_XMIN_INVALID: u16 = 0x0200;
pub(crate) const HEAP_XMIN_COMMITTED: u16 = 0x0100;
const HEAP_XMAX_LOCK_ONLY: u16 = 0x0080;
const HEAP_XMAX_EXCL_LOCK: u16 = 0x0040;
const HEAP_COMBOCID: u16 = 0x0020;
const HEAP_XMAX_KEYSHR_LOCK: u16 = 0x0010;
const HEAP_HASOID_OLD: u16 = 0x0008;
const HEAP_HASEXTERNAL: u16 = 0x0004;
const HEAP_HASVARWIDTH: u16 = 0x0002;
const HEAP_HASNULL: u16 = 0x0001;

impl TupleHeader {
    /// This version has no index entry of its own.
    ///
    /// A HOT update wrote it on the same page as the previous version, and
    /// index scans reach it by following `ctid` from the version that does
    /// have an entry. Postgres's `HEAP_ONLY_TUPLE`.
    /// See [Heap-Only Tuples](https://www.postgresql.org/docs/current/storage-hot.html).
    pub fn heap_only(&self) -> bool {
        (self.infomask2 & HEAP_ONLY_TUPLE) != 0
    }

    /// An update of this tuple changed no indexed column, and the new version
    /// is on the same page.
    ///
    /// The new version has no index entry of its own: index scans reach it by
    /// following `ctid` from this one. Postgres's `HEAP_HOT_UPDATED`.
    /// See [Heap-Only Tuples](https://www.postgresql.org/docs/current/storage-hot.html).
    pub fn hot_updated(&self) -> bool {
        (self.infomask2 & HEAP_HOT_UPDATED) != 0
    }

    /// `xmax` deleted the tuple, updated one of its key columns, or locked it
    /// `FOR UPDATE`.
    ///
    /// These conflict with `FOR KEY SHARE` locks, which foreign-key checks
    /// take. Postgres's `HEAP_KEYS_UPDATED`.
    pub fn keys_updated(&self) -> bool {
        (self.infomask2 & HEAP_KEYS_UPDATED) != 0
    }

    /// Number of columns stored in the tuple: the low 11 bits of
    /// [`Self::infomask2`].
    ///
    /// Columns added to the table after this row was written are absent, and
    /// read as NULL or as the default they were added with. Postgres's
    /// `HeapTupleHeaderGetNatts`.
    pub fn natts(&self) -> u16 {
        self.infomask2 & HEAP_NATTS_MASK
    }

    /// VACUUM FULL before Postgres 9.0 moved this tuple here from another page.
    ///
    /// [`Self::cid`]'s bytes hold that VACUUM's transaction (`t_xvac`). Kept so
    /// those pages still parse. Postgres's `HEAP_MOVED_IN`.
    pub fn moved_in(&self) -> bool {
        (self.infomask & HEAP_MOVED_IN) != 0
    }

    /// VACUUM FULL before Postgres 9.0 moved this tuple elsewhere.
    ///
    /// This copy is dead if that VACUUM committed. [`Self::cid`]'s bytes hold
    /// its transaction (`t_xvac`). Kept so those pages still parse.
    /// Postgres's `HEAP_MOVED_OFF`.
    pub fn moved_off(&self) -> bool {
        (self.infomask & HEAP_MOVED_OFF) != 0
    }

    /// This row version was created by an UPDATE.
    ///
    /// The previous version's `ctid` points here. Postgres's `HEAP_UPDATED`.
    pub fn updated(&self) -> bool {
        (self.infomask & HEAP_UPDATED) != 0
    }

    /// `xmax` is a multixact id, shared by several lockers, or by a locker
    /// and an updater.
    ///
    /// Postgres's `HEAP_XMAX_IS_MULTI`.
    pub fn xmax_is_multi(&self) -> bool {
        (self.infomask & HEAP_XMAX_IS_MULTI) != 0
    }

    /// Hint bit: `xmax` aborted, or no transaction has deleted or locked the
    /// tuple.
    ///
    /// A reader sets it after checking the commit log, so a later reader can
    /// skip that lookup. Postgres's `HEAP_XMAX_INVALID`.
    pub fn xmax_invalid(&self) -> bool {
        (self.infomask & HEAP_XMAX_INVALID) != 0
    }

    /// Hint bit: the `xmax` transaction committed.
    ///
    /// A reader sets it after checking the commit log. Postgres's
    /// `HEAP_XMAX_COMMITTED`.
    pub fn xmax_committed(&self) -> bool {
        (self.infomask & HEAP_XMAX_COMMITTED) != 0
    }

    /// Hint bit: the inserting transaction aborted.
    ///
    /// Set together with [`Self::xmin_committed`], the tuple is frozen:
    /// committed so long ago that every snapshot sees it. Postgres's
    /// `HEAP_XMIN_INVALID`.
    pub fn xmin_invalid(&self) -> bool {
        (self.infomask & HEAP_XMIN_INVALID) != 0
    }

    /// Hint bit: the inserting transaction committed.
    ///
    /// A reader sets it after checking the commit log, so a later reader can
    /// skip that lookup. Postgres's `HEAP_XMIN_COMMITTED`.
    pub fn xmin_committed(&self) -> bool {
        (self.infomask & HEAP_XMIN_COMMITTED) != 0
    }

    /// `xmax` locked the tuple and left its columns unchanged.
    ///
    /// Postgres's `HEAP_XMAX_LOCK_ONLY`.
    pub fn xmax_lock_only(&self) -> bool {
        (self.infomask & HEAP_XMAX_LOCK_ONLY) != 0
    }

    /// `xmax` holds an exclusive lock: `FOR UPDATE` or `FOR NO KEY UPDATE`.
    ///
    /// Postgres's `HEAP_XMAX_EXCL_LOCK`.
    pub fn xmax_excl_lock(&self) -> bool {
        (self.infomask & HEAP_XMAX_EXCL_LOCK) != 0
    }

    /// `cid` indexes the two command ids of one transaction that both inserted
    /// this tuple and then deleted or updated it.
    ///
    /// The two ids share this field's 4 bytes, so Postgres stores the pair in
    /// a map and keeps the index in `cid`. Postgres's `HEAP_COMBOCID`.
    pub fn combo_cid(&self) -> bool {
        (self.infomask & HEAP_COMBOCID) != 0
    }

    /// `xmax` holds a key-share lock (`FOR KEY SHARE`).
    ///
    /// Together with [`Self::xmax_excl_lock`], the lock is `FOR SHARE`.
    /// Postgres's `HEAP_XMAX_KEYSHR_LOCK`.
    pub fn xmax_keyshr_lock(&self) -> bool {
        (self.infomask & HEAP_XMAX_KEYSHR_LOCK) != 0
    }

    /// A 4-byte object id sits just before the columns, as user tables could
    /// store before Postgres 12.
    ///
    /// The bit remains so those pages still parse. Postgres's `HEAP_HASOID_OLD`.
    pub fn has_oid(&self) -> bool {
        (self.infomask & HEAP_HASOID_OLD) != 0
    }

    /// Some column is stored out of line, and the tuple holds a pointer to it.
    ///
    /// Postgres's `HEAP_HASEXTERNAL`.
    pub fn has_external(&self) -> bool {
        (self.infomask & HEAP_HASEXTERNAL) != 0
    }

    /// Some column is variable-length, so its width is stored with the value.
    ///
    /// Postgres's `HEAP_HASVARWIDTH`.
    pub fn has_varwidth(&self) -> bool {
        (self.infomask & HEAP_HASVARWIDTH) != 0
    }

    /// Some column is NULL, so a null bitmap follows the fixed header.
    ///
    /// Postgres's `HEAP_HASNULL`.
    pub fn has_null(&self) -> bool {
        (self.infomask & HEAP_HASNULL) != 0
    }
}
