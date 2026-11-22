//! Converts a heap tuple into a [`Row`].
//!
//! Only one specific Row type is supported for now: (Int8, Text, Text NOT NULL).

use super::{Tuple, TupleError, UnimplementedTuple};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub id: i64,
    pub payload: Option<String>,
    pub non_null_payload: String,
}

pub fn deform(tuple: &Tuple) -> Result<Row, TupleError> {
    let mut columns = Columns {
        tuple,
        column: 0,
        offset: 0,
    };
    let id = columns.next_int8_not_null()?;
    let payload = columns.next_text()?;
    let non_null_payload = columns.next_text_not_null()?;
    Ok(Row {
        id,
        payload,
        non_null_payload,
    })
}

/// An iterator-like structure yielding the columns of a tuple.
struct Columns<'t, 'a> {
    tuple: &'t Tuple<'a>,

    /// Index of the next column to read.
    column: usize,

    /// Current offset in bytes, in the tuple's columns data.
    offset: usize,
}

impl<'t, 'a> Columns<'t, 'a> {
    /// Moves on to the next column and indicates whether it is NULL, or returns `None` if there are
    /// no more columns.
    fn peek(&mut self) -> Option<bool> {
        if self.column >= self.tuple.header.natts() as usize {
            return None;
        }

        // One bit per column, lowest first. A set bit means the column is not NULL.
        let is_null = self
            .tuple
            .null_bitmap
            .is_some_and(|bitmap| bitmap[self.column / 8] & (1 << (self.column % 8)) == 0);
        self.column += 1;
        Some(is_null)
    }

    /// Reads the next column as a nullable int8. Fails if there are no more columns.
    fn next_int8(&mut self) -> Result<Option<i64>, TupleError> {
        match self.peek() {
            None => Err(TupleError::Truncated),
            Some(true) => Ok(None),
            Some(false) => {
                let start = self.offset.next_multiple_of(8);
                let bytes = self
                    .tuple
                    .columns_data
                    .get(start..start + 8)
                    .ok_or(TupleError::Truncated)?;
                self.offset = start + 8;
                Ok(Some(i64::from_le_bytes(bytes.try_into().unwrap())))
            }
        }
    }

    /// Reads the next column as a non-nullable int8. Fails if there are no more columns, or
    /// if the next column is NULL.
    fn next_int8_not_null(&mut self) -> Result<i64, TupleError> {
        self.next_int8()?.ok_or(TupleError::UnexpectedNull {
            attnum: self.column as u16,
        })
    }

    /// Reads the next column as a nullable text. Fails if there are no more columns, or if the
    /// value isn't valid UTF-8.
    fn next_text(&mut self) -> Result<Option<String>, TupleError> {
        self.next_varlena()?
            .map(|bytes| {
                std::str::from_utf8(bytes)
                    .map_err(|_| TupleError::InvalidUtf8)
                    .map(|s| s.to_owned())
            })
            .transpose()
    }

    /// Reads the next column as a non-nullable text. Fails if there are no more columns, if the
    /// next column is NULL, or if the value isn't valid UTF-8.
    fn next_text_not_null(&mut self) -> Result<String, TupleError> {
        self.next_text()?.ok_or(TupleError::UnexpectedNull {
            attnum: self.column as u16,
        })
    }

    /// Reads the next column as a variable-length array of bytes (a varlena). Fails if there are
    /// no more columns, if the column's bytes don't form a valid varlena, or if they form one
    /// that bonsai doesn't support yet.
    fn next_varlena(&mut self) -> Result<Option<&[u8]>, TupleError> {
        if self.peek().ok_or(TupleError::Truncated)? {
            return Ok(None);
        }

        // The header indicating the length may be 1 or 4 bytes long. This means that we need to
        // check the value of the current byte to determine whether we need to realign: We need to
        // realign if and only if the current byte is 0x00.
        self.offset = if *self
            .tuple
            .columns_data
            .get(self.offset)
            .ok_or(TupleError::Truncated)?
            == 0x00
        {
            self.offset.next_multiple_of(4)
        } else {
            self.offset
        };

        let first_byte = self
            .tuple
            .columns_data
            .get(self.offset)
            .ok_or(TupleError::Truncated)?;

        // See https://github.com/postgres/postgres/blob/master/src/include/varatt.h#L211-L223
        let (header_len, data_len) = match first_byte {
            0x01 => return Err(TupleError::Unimplemented(UnimplementedTuple::Toasted)),
            one_terminated @ x if x % 2 == 1 => (1, (one_terminated >> 1) as usize - 1),
            zero_terminated => {
                if zero_terminated & 0x02 == 0x02 {
                    return Err(TupleError::Unimplemented(UnimplementedTuple::Compressed));
                } else {
                    let bytes = self
                        .tuple
                        .columns_data
                        .get(self.offset..self.offset + 4)
                        .ok_or(TupleError::Truncated)?;
                    let word = u32::from_le_bytes(bytes.try_into().unwrap());
                    let len = ((word >> 2) as usize)
                        .checked_sub(4)
                        .ok_or(TupleError::Truncated)?;
                    (4, len)
                }
            }
        };
        self.offset += header_len;

        let data = self
            .tuple
            .columns_data
            .get(self.offset..self.offset + data_len)
            .ok_or(TupleError::Truncated)?;
        self.offset = self.offset + data_len;
        Ok(Some(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::heap::tuple::TupleHeader;

    /// A tuple of `natts` columns laid out in `columns_data`, with `null_bitmap` if some are NULL.
    fn tuple<'a>(natts: u16, null_bitmap: Option<&'a [u8]>, columns_data: &'a [u8]) -> Tuple<'a> {
        Tuple {
            header: TupleHeader {
                xmin: None,
                xmax: None,
                cid: 0,
                ctid: None,
                infomask2: natts,
                infomask: 0,
                header_size: 24,
            },
            null_bitmap,
            columns_data,
        }
    }

    fn columns<'t, 'a>(tuple: &'t Tuple<'a>) -> Columns<'t, 'a> {
        Columns {
            tuple,
            column: 0,
            offset: 0,
        }
    }

    /// `value` behind a 1-byte varlena header.
    fn short_varlena(value: &[u8]) -> Vec<u8> {
        let mut bytes = vec![(((value.len() + 1) << 1) | 1) as u8];
        bytes.extend(value);
        bytes
    }

    /// `value` behind a 4-byte varlena header.
    fn long_varlena(value: &[u8]) -> Vec<u8> {
        let mut bytes = (((value.len() + 4) << 2) as u32).to_le_bytes().to_vec();
        bytes.extend(value);
        bytes
    }

    #[test]
    fn a_nullable_text_then_a_non_nullable_text() {
        let long = "x".repeat(200);
        // A 1-byte header needs no alignment, so the first value ends at offset 3. The second
        // value's 4-byte header is aligned to 4, after one byte of padding.
        let mut data = short_varlena(b"ab");
        data.push(0);
        data.extend(long_varlena(long.as_bytes()));
        let tuple = tuple(2, None, &data);
        let mut columns = columns(&tuple);
        assert_eq!(columns.next_text(), Ok(Some("ab".to_owned())));
        assert_eq!(columns.next_text_not_null(), Ok(long));
        assert_eq!(columns.offset, data.len());
    }

    #[test]
    fn short_texts_follow_each_other_without_padding() {
        let mut data = short_varlena(b"ab");
        data.extend(short_varlena("café".as_bytes()));
        let tuple = tuple(2, None, &data);
        let mut columns = columns(&tuple);
        assert_eq!(columns.next_text(), Ok(Some("ab".to_owned())));
        assert_eq!(columns.next_text_not_null(), Ok("café".to_owned()));
    }

    #[test]
    fn a_null_text_takes_no_bytes() {
        // Bit 0 is clear: the first column is NULL, and the data starts with the second.
        let data = short_varlena(b"ab");
        let tuple = tuple(2, Some(&[0b10]), &data);
        let mut columns = columns(&tuple);
        assert_eq!(columns.next_text(), Ok(None));
        assert_eq!(columns.next_text_not_null(), Ok("ab".to_owned()));
    }

    #[test]
    fn a_null_where_none_is_allowed_names_its_column() {
        let data = short_varlena(b"ab");
        let tuple = tuple(2, Some(&[0b01]), &data);
        let mut columns = columns(&tuple);
        assert_eq!(columns.next_text(), Ok(Some("ab".to_owned())));
        assert_eq!(
            columns.next_text_not_null(),
            Err(TupleError::UnexpectedNull { attnum: 2 })
        );
    }

    #[test]
    fn the_empty_text_is_only_a_header() {
        let tuple = tuple(1, None, &[0x03]);
        assert_eq!(columns(&tuple).next_text_not_null(), Ok(String::new()));
    }

    #[test]
    fn text_must_be_utf8() {
        let data = short_varlena(&[0xFF, 0xFE]);
        let tuple = tuple(1, None, &data);
        assert_eq!(columns(&tuple).next_text(), Err(TupleError::InvalidUtf8));
    }

    #[test]
    fn toasted_and_compressed_texts_are_unimplemented() {
        // 0x01, then the tag of a pointer to a value moved out to TOAST.
        let toasted = tuple(1, None, &[0x01, 18]);
        assert_eq!(
            columns(&toasted).next_text(),
            Err(TupleError::Unimplemented(UnimplementedTuple::Toasted))
        );
        // A 4-byte header whose second lowest bit is set.
        let header = ((12u32 << 2) | 0b10).to_le_bytes();
        let compressed = tuple(1, None, &header);
        assert_eq!(
            columns(&compressed).next_text(),
            Err(TupleError::Unimplemented(UnimplementedTuple::Compressed))
        );
    }

    #[test]
    fn lengths_that_dont_fit_are_truncated() {
        // A 4-byte header that claims a total length of 2.
        let header = (2u32 << 2).to_le_bytes();
        let too_short = tuple(1, None, &header);
        assert_eq!(columns(&too_short).next_text(), Err(TupleError::Truncated));

        let mut data = short_varlena(b"abc");
        data.pop();
        let too_long = tuple(1, None, &data);
        assert_eq!(columns(&too_long).next_text(), Err(TupleError::Truncated));
    }

    #[test]
    fn reading_past_the_last_column_fails() {
        let tuple = tuple(0, None, &[]);
        assert_eq!(columns(&tuple).next_text(), Err(TupleError::Truncated));
    }

    #[test]
    fn the_ninth_column_is_in_the_bitmaps_second_byte() {
        let tuple = tuple(9, Some(&[0xFF, 0x00]), &[]);
        let mut columns = columns(&tuple);
        for _ in 0..8 {
            assert_eq!(columns.peek(), Some(false));
        }
        assert_eq!(columns.peek(), Some(true));
        assert_eq!(columns.peek(), None);
    }

    #[test]
    fn deform_reads_an_id_a_nullable_payload_and_a_non_null_payload() {
        let id = 5i64.to_le_bytes();
        let payload = short_varlena(b"hi");
        let non_null_payload = short_varlena(b"there");
        // The columns' bytes, without those of the NULL ones.
        let data = |columns: &[&[u8]]| columns.concat();

        let all = data(&[&id, &payload, &non_null_payload]);
        let row = deform(&tuple(3, None, &all)).unwrap();
        assert_eq!(
            (row.id, row.payload.as_deref(), row.non_null_payload.as_str()),
            (5, Some("hi"), "there")
        );

        let null_payload = data(&[&id, &non_null_payload]);
        let row = deform(&tuple(3, Some(&[0b101]), &null_payload)).unwrap();
        assert_eq!(
            (row.id, row.payload, row.non_null_payload.as_str()),
            (5, None, "there")
        );

        let null_id = data(&[&payload, &non_null_payload]);
        assert_eq!(
            deform(&tuple(3, Some(&[0b110]), &null_id)).err(),
            Some(TupleError::UnexpectedNull { attnum: 1 })
        );

        let null_non_null_payload = data(&[&id, &payload]);
        assert_eq!(
            deform(&tuple(3, Some(&[0b011]), &null_non_null_payload)).err(),
            Some(TupleError::UnexpectedNull { attnum: 3 })
        );
    }
}
