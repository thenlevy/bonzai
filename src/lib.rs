//! bonsai, a small B-tree engine built to understand how Postgres stores,
//! indexes and recovers data.

use std::num::{NonZeroU32, NonZeroU64};

pub mod storage;

/// A position in the write-ahead log, in bytes from its start: a Log Sequence
/// Number (LSN). Postgres's `XLogRecPtr`, without its invalid value 0
/// (`InvalidXLogRecPtr`, in
/// [xlogdefs.h](https://github.com/postgres/postgres/blob/master/src/include/access/xlogdefs.h)).
/// See [WAL Internals](https://www.postgresql.org/docs/current/wal-internals.html).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct XLogRecPtr(
    // Postgres uses the value 0 (`InvalidXLogRecPtr`) to indicate an invalid XLogRecPtr.
    NonZeroU64,
);

impl XLogRecPtr {
    /// The LSN stored as `raw`, or `None` if `raw` is 0.
    pub fn from_raw(raw: u64) -> Option<Self> {
        NonZeroU64::new(raw).map(XLogRecPtr)
    }
}

impl From<XLogRecPtr> for u64 {
    fn from(lsn: XLogRecPtr) -> Self {
        lsn.0.get()
    }
}

const _: () = assert!(size_of::<Option<XLogRecPtr>>() == size_of::<u64>());

/// Identifies a transaction. See
/// [transam.h](https://github.com/postgres/postgres/blob/master/src/include/access/transam.h).
///
/// XIDs wrap around after 2^32, and are compared "on a circle" (`TransactionIdPrecedes`):
/// An XID counts the 2^31 IDs after it as newer and the ones before it as older.
///
/// See [Transactions and Identifiers](https://www.postgresql.org/docs/current/transaction-id.html)
/// and [Preventing Transaction ID Wraparound Failures](https://www.postgresql.org/docs/current/routine-vacuuming.html#VACUUM-FOR-WRAPAROUND).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TransactionId(
    // Postgres uses the value 0 (`InvalidTransactionId`) to indicate an invalid transaction ID
    NonZeroU32,
);

impl TransactionId {
    /// The ID stored as `raw`, or `None` if `raw` is 0.
    pub fn from_raw(raw: u32) -> Option<Self> {
        NonZeroU32::new(raw).map(TransactionId)
    }
}

impl From<TransactionId> for u32 {
    fn from(xid: TransactionId) -> Self {
        xid.0.get()
    }
}

// 0 can't be a `NonZeroU32`, so `Option` uses it for `None` and costs no space.
const _: () = assert!(size_of::<Option<TransactionId>>() == size_of::<u32>());
