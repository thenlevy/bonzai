//! Sequential scans: every row of a heap in TID order.

use std::num::NonZeroU16;

use crate::{
    heap::tuple::{
        TupleError, UnimplementedTuple,
        deform::{Row, deform},
    },
    storage::{
        BlockCount, BlockNumber,
        buffer::{BufferManager, PageGuard, ReadError},
        item::{InvalidItemError, LinePointer, LinePointerNumber, Tid},
        manager::{BlockIdentifier, ForkError, ForkIdentifier, ForkKind, RelFileNumber},
        page::InvalidHeader,
    },
};

pub struct SeqScan<'a> {
    buffers: &'a BufferManager,
    fork: ForkIdentifier,
    block_count: BlockCount,
    /// The number of the next block to read.
    next_block_number: BlockNumber,
    /// The page being scanned, and the next line pointer to look at.
    current: Option<(PageGuard<'a>, LinePointerNumber)>,
    done: bool,
}

#[derive(Debug)]
pub enum ScanError {
    /// The heap's main fork can't be opened.
    Fork(ForkError),
    Read(ReadError),
    /// The header of block `block_number` doesn't describe its line pointers.
    Header {
        block_number: BlockNumber,
        error: InvalidHeader,
    },
    /// The line pointer at `tid` points outside the page's tuples.
    LinePointer {
        tid: Tid,
        error: InvalidItemError,
    },
    Tuple {
        tid: Tid,
        error: TupleError,
    },
}

impl<'a> SeqScan<'a> {
    pub fn new(buffers: &'a BufferManager, rel: RelFileNumber) -> Result<Self, ScanError> {
        let fork = ForkIdentifier {
            rel,
            kind: ForkKind::Main,
        };
        let block_count = buffers.block_count(fork).map_err(ScanError::Fork)?;
        Ok(SeqScan {
            buffers,
            fork,
            block_count,
            next_block_number: BlockNumber(0),
            current: None,
            done: false,
        })
    }

    fn next_row(&mut self) -> Result<Option<(Tid, Row)>, ScanError> {
        loop {
            let Some((page, next)) = &mut self.current else {
                if !self.block_count.contains(self.next_block_number) {
                    return Ok(None);
                }
                let block = BlockIdentifier {
                    fork: self.fork,
                    block_number: self.next_block_number,
                };
                let page = self
                    .buffers
                    .validated_read(block)
                    .map_err(ScanError::Read)?;
                self.current = Some((page, LinePointerNumber(NonZeroU16::MIN)));
                self.next_block_number.0 += 1;
                continue;
            };
            let block_number = page.block().block_number;
            let header_error = |error| ScanError::Header {
                block_number,
                error,
            };

            // We go to the next page if the current is empty (max_line_pointer_number is None) or
            // if we have read it entirely (next > max_line_pointer_number).
            let max_line_pointer_number = page.max_line_pointer_number().map_err(header_error)?;
            if max_line_pointer_number.is_none_or(|max| *next > max) {
                self.current = None;
                continue;
            }

            let tid = Tid {
                block_number,
                line_pointer_number: *next,
            };
            next.0 = next.0.saturating_add(1);
            let line_pointer = page
                .line_pointer(tid.line_pointer_number)
                .map_err(header_error)?;

            // A LinePointer::Redirect leads to a newer version in a HOT chain, which has a line
            // pointer of its own that the scan reaches anyway.
            let Some(LinePointer::Normal { offset, len }) = line_pointer else {
                continue;
            };
            let page = page.with_validated_header().map_err(header_error)?;
            let item = page
                .item(offset, len)
                .map_err(|error| ScanError::LinePointer { tid, error })?;

            let tuple_error = |error| ScanError::Tuple { tid, error };
            let tuple = item.try_into_heap_tuple().map_err(tuple_error)?;

            // For now we only support cases where all tuples are live rows:
            // * All transactions have been committed
            // * There was no deletion or replacement of the row by a newer version
            let header = tuple.header();
            if !(header.xmin_committed() && header.xmax_invalid()) {
                return Err(tuple_error(TupleError::Unimplemented(
                    UnimplementedTuple::UnsupportedVisibility {
                        infomask: header.infomask,
                    },
                )));
            }
            let row = deform(&tuple).map_err(tuple_error)?;
            return Ok(Some((tid, row)));
        }
    }
}

/// Yields each row of the heap with its TID, in TID order, and stops after the first error.
impl Iterator for SeqScan<'_> {
    type Item = Result<(Tid, Row), ScanError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let next = self.next_row().transpose();
        self.done = !matches!(next, Some(Ok(_)));
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use crate::{
        heap::tuple::{
            FIXED_HEADER_SIZE_BYTES, HEAP_XMAX_INVALID, HEAP_XMIN_COMMITTED, HEAP_XMIN_INVALID,
        },
        storage::{
            BLCKSZ_BYTES,
            item::LINE_POINTER_SIZE_BYTES,
            manager::StorageManager,
            page::{MAXIMUM_ALIGNOF_BYTES, Page, PageOffset, PageValidationError},
        },
    };

    const REL: RelFileNumber = RelFileNumber(16384);

    /// Hint bits of a row inserted by a committed transaction and never deleted.
    const LIVE: u16 = HEAP_XMIN_COMMITTED | HEAP_XMAX_INVALID;

    fn tid(block_number: u32, line_pointer_number: u16) -> Tid {
        Tid {
            block_number: BlockNumber(block_number),
            line_pointer_number: LinePointerNumber(NonZeroU16::new(line_pointer_number).unwrap()),
        }
    }

    fn row(id: i64) -> Row {
        Row {
            id,
            payload: Some(format!("row {id}")),
            non_null_payload: format!("not null {id}"),
        }
    }

    /// `value` behind a 1-byte varlena header.
    fn short_varlena(value: &[u8]) -> Vec<u8> {
        let mut bytes = vec![(((value.len() + 1) << 1) | 1) as u8];
        bytes.extend(value);
        bytes
    }

    /// The tuple of `row(id)`, inserted by transaction 100, with `infomask`.
    fn tuple(infomask: u16, id: i64) -> Vec<u8> {
        let row = row(id);
        let mut bytes = vec![0; FIXED_HEADER_SIZE_BYTES];
        bytes[0..4].copy_from_slice(&100u32.to_le_bytes());
        bytes[18..20].copy_from_slice(&3u16.to_le_bytes());
        bytes[20..22].copy_from_slice(&infomask.to_le_bytes());
        let header_size = FIXED_HEADER_SIZE_BYTES.next_multiple_of(MAXIMUM_ALIGNOF_BYTES);
        bytes.resize(header_size, 0);
        bytes[22] = header_size as u8;
        bytes.extend(row.id.to_le_bytes());
        bytes.extend(short_varlena(row.payload.unwrap().as_bytes()));
        bytes.extend(short_varlena(row.non_null_payload.as_bytes()));
        bytes
    }

    fn live(id: i64) -> Vec<u8> {
        tuple(LIVE, id)
    }

    /// What a line pointer of a hand-built page holds.
    enum Slot<'a> {
        Tuple(&'a [u8]),
        Unused,
        Dead,
        Redirect(u16),
        /// A line pointer word, as is.
        Raw(u32),
    }

    /// A heap page holding `slots`, with its checksum set for `block_number`.
    fn page(block_number: u32, slots: &[Slot]) -> Page {
        let mut page = Page {
            bytes: [0; BLCKSZ_BYTES],
        };
        Page::init(&mut page, 0);
        let mut header = page.header().unwrap();
        for slot in slots {
            let raw = match *slot {
                Slot::Tuple(tuple) => {
                    let start = usize::from(header.upper.0)
                        - tuple.len().next_multiple_of(MAXIMUM_ALIGNOF_BYTES);
                    page[start..start + tuple.len()].copy_from_slice(tuple);
                    header.upper = PageOffset(start as u16);
                    start as u32 | (1 << 15) | ((tuple.len() as u32) << 17)
                }
                Slot::Unused => 0,
                Slot::Dead => 3 << 15,
                Slot::Redirect(target) => u32::from(target) | (2 << 15),
                Slot::Raw(raw) => raw,
            };
            let lower = usize::from(header.lower.0);
            page[lower..lower + LINE_POINTER_SIZE_BYTES].copy_from_slice(&raw.to_le_bytes());
            header.lower = PageOffset((lower + LINE_POINTER_SIZE_BYTES) as u16);
        }
        page.set_header(header);
        let sealed = page
            .try_set_checksum_for_block(BlockNumber(block_number))
            .unwrap();
        sealed.page().clone()
    }

    /// Scans a heap made of `pages`.
    fn scan(pages: &[Page]) -> Vec<Result<(Tid, Row), ScanError>> {
        let dir = tempfile::tempdir().unwrap();
        let bytes: Vec<u8> = pages.iter().flat_map(|page| page.bytes).collect();
        fs::write(dir.path().join("16384"), bytes).unwrap();
        let buffers = BufferManager::new(StorageManager::open(dir.path()).unwrap());
        SeqScan::new(&buffers, REL).unwrap().collect()
    }

    fn rows(pages: &[Page]) -> Vec<(Tid, Row)> {
        scan(pages).into_iter().map(Result::unwrap).collect()
    }

    #[test]
    fn rows_come_in_tid_order_across_pages() {
        let pages = [
            page(0, &[Slot::Tuple(&live(1)), Slot::Tuple(&live(2))]),
            // Extending a relation leaves new pages all zeros until used.
            Page {
                bytes: [0; BLCKSZ_BYTES],
            },
            page(2, &[Slot::Tuple(&live(3))]),
        ];
        assert_eq!(
            rows(&pages),
            [
                (tid(0, 1), row(1)),
                (tid(0, 2), row(2)),
                (tid(2, 1), row(3))
            ]
        );
    }

    #[test]
    fn an_empty_heap_has_no_rows() {
        assert!(rows(&[]).is_empty());
    }

    #[test]
    fn line_pointers_without_a_tuple_are_skipped() {
        let slots = [
            Slot::Tuple(&live(1)),
            Slot::Unused,
            Slot::Dead,
            Slot::Redirect(5),
            Slot::Tuple(&live(5)),
        ];
        assert_eq!(
            rows(&[page(0, &slots)]),
            [(tid(0, 1), row(1)), (tid(0, 5), row(5))]
        );
    }

    #[test]
    fn rows_that_need_the_commit_log_are_refused() {
        for infomask in [
            // Nothing hinted: the inserting transaction may still be running.
            0,
            // Aborted insert.
            HEAP_XMIN_INVALID | HEAP_XMAX_INVALID,
            // Deleted, or at least locked.
            HEAP_XMIN_COMMITTED,
        ] {
            let bad = tuple(infomask, 2);
            let slots = [
                Slot::Tuple(&live(1)),
                Slot::Tuple(&bad),
                Slot::Tuple(&live(3)),
            ];
            let results = scan(&[page(0, &slots)]);
            assert_eq!(results.len(), 2, "the scan stops at the error");
            let Err(ScanError::Tuple {
                tid: bad_tid,
                error,
            }) = &results[1]
            else {
                panic!("expected a tuple error, got {:?}", results[1]);
            };
            assert_eq!(*bad_tid, tid(0, 2));
            assert_eq!(
                *error,
                TupleError::Unimplemented(UnimplementedTuple::UnsupportedVisibility { infomask })
            );
        }
        // Frozen: committed so long ago that every transaction sees it.
        let frozen = tuple(LIVE | HEAP_XMIN_INVALID, 1);
        assert_eq!(
            rows(&[page(0, &[Slot::Tuple(&frozen)])]),
            [(tid(0, 1), row(1))]
        );
    }

    #[test]
    fn a_line_pointer_outside_the_tuples_is_an_error() {
        // A normal line pointer to 40 bytes at offset 8, in the page header.
        let raw = 8 | (1 << 15) | (40 << 17);
        let results = scan(&[page(0, &[Slot::Raw(raw)])]);
        let [Err(ScanError::LinePointer { tid: bad_tid, .. })] = &results[..] else {
            panic!("expected a line pointer error, got {results:?}");
        };
        assert_eq!(*bad_tid, tid(0, 1));
    }

    #[test]
    fn a_page_that_fails_its_checksum_stops_the_scan() {
        // A page sealed for block 1, stored as block 0, then a good page.
        let pages = [
            page(1, &[Slot::Tuple(&live(1))]),
            page(1, &[Slot::Tuple(&live(2))]),
        ];
        let results = scan(&pages);
        assert!(matches!(
            results[..],
            [Err(ScanError::Read(ReadError::Page(
                PageValidationError::ChecksumMismatch { .. }
            )))]
        ));
    }
}
