//! Convert Heap tupple into Row.
//!
//! Only one specific Row type is supported for now: (Int8, Text).

use super::{Tuple, TupleError, UnimplementedTuple};

pub struct Row {
    id: i64,
    payload: Option<String>,
}

pub fn deform(tuple: &Tuple) -> Result<Row, TupleError> {
    let mut columns = Columns {
        tuple,
        column: 0,
        offset: 0,
    };
    let id = columns.next_int8_not_null()?;
    let payload = columns.next_text()?;
    Ok(Row { id, payload })
}

/// An iterator-like structure yielding the columns of a tuple.
struct Columns<'t, 'a> {
    tuple: &'t Tuple<'a>,

    /// Index of the last column read.
    column: usize,

    /// Current offset in bytes, in the tupple's columns data.
    offset: usize,
}

impl<'t, 'a> Columns<'t, 'a> {
    /// Moves on to the next column and indicate whether it is NULL, or returns `None` if there are
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
    /// or if the next column is NULL.
    fn next_int8_not_null(&mut self) -> Result<i64, TupleError> {
        self.next_int8()?.ok_or(TupleError::UnexpectedNull {
            attnum: self.column as u16,
        })
    }

    fn next_text(&mut self) -> Result<Option<String>, TupleError> {
        self.next_varlena()?
            .map(|bytes| {
                std::str::from_utf8(bytes)
                    .map_err(|_| TupleError::InvalidUtf8)
                    .map(|s| s.to_owned())
            })
            .transpose()
    }

    /// Reads the next column as a variable length array of bytes. Fails if there are no more
    /// columns, or if the byte patterns of the next column data does not match a varlena, or is
    /// currently unsupported by bonsai.
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
