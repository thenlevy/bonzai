//! Mine vs theirs: a heap a real Postgres wrote, scanned through bonsai and
//! compared with what Postgres's `SELECT` and pageinspect returned. Build the
//! cluster first, from the dev shell:
//!
//! ```sh
//! tests/fixtures/make-db.sh
//! ```

mod common;

use std::{num::NonZeroU16, path::Path};

use bonsai::{
    TransactionId,
    heap::{scan::SeqScan, tuple::deform::Row},
    storage::{
        BlockNumber,
        buffer::BufferManager,
        item::{LinePointer, LinePointerNumber, Tid},
        manager::{BlockIdentifier, ForkIdentifier, ForkKind, RelFileNumber, StorageManager},
        page::Page,
    },
};
use common::{Facts, POSTGRES_DIR, read};

/// The relation a fixture script created, and a buffer manager over its
/// database. The script prints the relation's path as the fact `path`.
fn open(fixture: &str) -> (BufferManager, RelFileNumber) {
    let path: String = Facts::read(fixture).get("path");
    let path = Path::new(&path);
    let rel = path.file_name().unwrap().to_str().unwrap().parse().unwrap();
    let dir = Path::new(POSTGRES_DIR)
        .join("data")
        .join(path.parent().unwrap());
    let smgr = StorageManager::open(dir).unwrap();
    (BufferManager::new(smgr), RelFileNumber(rel))
}

#[test]
fn a_sequential_scan_returns_what_select_returned() {
    let expected: Vec<(Tid, Row)> = copy_rows("heap-scan.rows")
        .into_iter()
        .map(|fields| {
            let [tid, id, payload, non_null_payload] = fields.try_into().unwrap();
            let row = Row {
                id: id.unwrap().parse().unwrap(),
                payload,
                non_null_payload: non_null_payload.unwrap(),
            };
            (parse_tid(&tid.unwrap()), row)
        })
        .collect();

    // The fixture has what the scan must get right.
    let has = |what: &str, case: &dyn Fn(&Row) -> bool| {
        assert!(expected.iter().any(|(_, row)| case(row)), "no {what}");
    };
    assert!(
        expected.last().unwrap().0.block > BlockNumber(1),
        "a single page"
    );
    has("NULL", &|row| row.payload.is_none());
    has("empty text", &|row| row.payload.as_deref() == Some(""));
    has("4-byte varlena header", &|row| {
        row.payload.as_ref().is_some_and(|payload| payload.len() > 126)
    });
    has("multi-byte character", &|row| {
        row.payload.as_ref().is_some_and(|payload| !payload.is_ascii())
    });
    has("empty non-NULL text", &|row| row.non_null_payload.is_empty());
    // `id` takes bytes 0 to 8, then a `payload` with a 1-byte header.
    let payload_end = |row: &Row| row.payload.as_ref().map(|payload| 8 + 1 + payload.len());
    has("4-byte header after padding", &|row| {
        payload_end(row).is_some_and(|end| end % 4 != 0 && end <= 8 + 127)
            && row.non_null_payload.len() > 126
    });
    has("1-byte header at an unaligned offset", &|row| {
        payload_end(row).is_some_and(|end| end % 4 != 0 && end <= 8 + 127)
            && (1..=126).contains(&row.non_null_payload.len())
    });

    let (buffers, rel) = open("heap-scan");
    let rows: Vec<(Tid, Row)> = SeqScan::new(&buffers, rel)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for (row, expected) in rows.iter().zip(&expected) {
        assert_eq!(row, expected);
    }
    assert_eq!(rows.len(), expected.len());
}

/// Every line pointer and tuple header of the fixture's heap, next to what
/// pageinspect's `heap_page_items` read in the same pages.
#[test]
fn line_pointers_and_tuple_headers_read_like_pageinspect() {
    let expected = copy_rows("heap-scan.items");
    let (buffers, rel) = open("heap-scan");
    let fork = ForkIdentifier {
        rel,
        kind: ForkKind::Main,
    };
    let nb_blocks = buffers.nb_blocks(fork).unwrap();
    let items: Vec<_> = (0..nb_blocks.0)
        .flat_map(|block_number| {
            let block = BlockIdentifier {
                fork,
                block_number: BlockNumber(block_number),
            };
            let page = buffers.validated_read(block).unwrap();
            let max = page.max_line_pointer_number().unwrap().map_or(0, |max| max.0.get());
            (1..=max)
                .map(|line_pointer_number| {
                    let line_pointer_number =
                        LinePointerNumber(NonZeroU16::new(line_pointer_number).unwrap());
                    heap_page_item(&page, block_number, line_pointer_number)
                })
                .collect::<Vec<_>>()
        })
        .collect();
    for (item, expected) in items.iter().zip(&expected) {
        assert_eq!(item, expected);
    }
    assert_eq!(items.len(), expected.len());
}

/// The columns of `heap_page_items` the fixture dumps, for one line pointer,
/// as Postgres prints them: `blkno`, `lp`, `lp_off`, `lp_flags`, `lp_len`,
/// then the tuple header if the line pointer holds a tuple: `t_xmin`,
/// `t_xmax`, `t_field3`, `t_ctid`, `t_infomask2`, `t_infomask` and `t_hoff`.
fn heap_page_item(
    page: &Page,
    block_number: u32,
    line_pointer_number: LinePointerNumber,
) -> Vec<Option<String>> {
    let line_pointer = page.line_pointer(line_pointer_number).unwrap().unwrap();
    let (lp_off, lp_flags, lp_len) = match line_pointer {
        LinePointer::Unused => (0, 0, 0),
        LinePointer::Normal { offset, len } => (offset.0, 1, len),
        LinePointer::Redirect(target) => (target.map_or(0, |target| target.0.get()), 2, 0),
        LinePointer::Dead { offset, len } => (offset.0, 3, len),
    };
    let mut item: Vec<_> = [
        block_number,
        line_pointer_number.0.get().into(),
        lp_off.into(),
        lp_flags,
        lp_len.into(),
    ]
    .map(|value| Some(value.to_string()))
    .into();
    let LinePointer::Normal { offset, len } = line_pointer else {
        item.resize(12, None);
        return item;
    };
    let page = page.with_validated_header().unwrap();
    let tuple = page.item(offset, len).unwrap().try_into_heap_tuple().unwrap();
    let header = tuple.header();
    let xid = |xid: Option<TransactionId>| xid.map_or(0, u32::from).to_string();
    item.extend(
        [
            xid(header.xmin),
            xid(header.xmax),
            header.cid.to_string(),
            header.ctid.unwrap().to_string(),
            header.infomask2.to_string(),
            header.infomask.to_string(),
            header.hoff.to_string(),
        ]
        .map(Some),
    );
    item
}

/// Reads a file COPY wrote in its text format: one line per row,
/// tab-separated columns, `\N` for NULL.
fn copy_rows(path: &str) -> Vec<Vec<Option<String>>> {
    let text = String::from_utf8(read(path)).unwrap();
    let rows: Vec<Vec<_>> = text
        .lines()
        .map(|line| {
            line.split('\t')
                .map(|field| (field != "\\N").then(|| unescape(field)))
                .collect()
        })
        .collect();
    assert!(!rows.is_empty(), "{path} is empty");
    rows
}

/// Undoes COPY's backslash escapes, those the fixtures can produce.
fn unescape(field: &str) -> String {
    let mut text = String::new();
    let mut chars = field.chars();
    while let Some(char) = chars.next() {
        if char != '\\' {
            text.push(char);
            continue;
        }
        match chars.next() {
            Some('\\') => text.push('\\'),
            Some('t') => text.push('\t'),
            Some('n') => text.push('\n'),
            Some('r') => text.push('\r'),
            other => panic!("unexpected escape \\{other:?} in {field:?}"),
        }
    }
    text
}

/// Parses a TID as Postgres prints it: `(block,line pointer number)`.
fn parse_tid(text: &str) -> Tid {
    let (block, line_pointer_number) = text
        .strip_prefix('(')
        .and_then(|text| text.strip_suffix(')'))
        .and_then(|text| text.split_once(','))
        .unwrap_or_else(|| panic!("not a TID: {text}"));
    Tid {
        block: BlockNumber(block.parse().unwrap()),
        line_pointer_number: LinePointerNumber(line_pointer_number.parse().unwrap()),
    }
}
