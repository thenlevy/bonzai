//! AI-generated translation of Postgres's [`checksum_impl.h`](https://github.com/postgres/postgres/blob/master/src/include/storage/checksum_impl.h).

use super::{
    BLCKSZ, BlockNumber,
    page::{PD_CHECKSUM, Page},
};

const N_SUMS: usize = 32;
const FNV_PRIME: u32 = 16_777_619;

/// Starting state of each column, picked at random by Postgres.
const BASE_OFFSETS: [u32; N_SUMS] = [
    0x5B1F36E9, 0xB8525960, 0x02AB50AA, 0x1DE66D2A, 0x79FF467A, 0x9BB9F8A3, 0x217E7CD2, 0x83E13D2C,
    0xF8D4474F, 0xE39EB970, 0x42C6AE16, 0x993216FA, 0x7B093B5D, 0x98DAFF3C, 0xF718902A, 0x0B1C9CDB,
    0xE58F764B, 0x187636BC, 0x5D7B3BB1, 0xE73DE7DE, 0x92BEC979, 0xCCA6C0B2, 0x304A0979, 0x85AA43D4,
    0x783125BB, 0x6CA8EAA2, 0xE407EAC6, 0x4B5CFC3E, 0x9FBF8C76, 0x15CA20BE, 0xF2CA9FD3, 0x959BD756,
];

fn mix(sum: u32, value: u32) -> u32 {
    let tmp = sum ^ value;
    tmp.wrapping_mul(FNV_PRIME) ^ (tmp >> 17)
}

fn checksum_block(page: &Page) -> u32 {
    let mut sums = BASE_OFFSETS;
    for (i, row) in page.chunks_exact(4 * N_SUMS).enumerate() {
        for (j, (sum, word)) in sums.iter_mut().zip(row.chunks_exact(4)).enumerate() {
            let mut value = u32::from_le_bytes(word.try_into().unwrap());
            // Bytes 8..12: pd_checksum in the low half, pd_flags in the high
            // half. The page's own checksum must read as zero.
            if i == 0 && j == 2 {
                value &= 0xFFFF_0000;
            }
            *sum = mix(*sum, value);
        }
    }
    // Two rounds of zeros mix the last words in as well as the others.
    for _ in 0..2 {
        for sum in &mut sums {
            *sum = mix(*sum, 0);
        }
    }
    sums.iter().fold(0, |result, sum| result ^ sum)
}

/// The checksum `page` should carry when stored at `block`. The page's own
/// `pd_checksum` field is read as zero.
pub fn checksum_page(page: &Page, block: BlockNumber) -> u16 {
    // Mixing in the block number catches a valid page written to the wrong place.
    let checksum = checksum_block(page) ^ block.0;
    // Fold to 16 bits, plus one so that a checksum is never zero.
    (checksum % 65535 + 1) as u16
}

const _: () = assert!(BLCKSZ.is_multiple_of(4 * N_SUMS));

// The mask in `checksum_block` assumes `pd_checksum` is bytes 8..10.
const _: () = assert!(PD_CHECKSUM.start == 8 && PD_CHECKSUM.end == 10);
