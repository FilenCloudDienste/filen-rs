use alloc::vec::Vec;

use crate::encoding::{compress_to_vec, CompressionLevel};

#[test]
fn fastest_does_not_expand_incompressible_blocks_past_raw_size() {
    assert_fastest_does_not_exceed_raw(8 * 1024);
}

#[test]
fn fastest_does_not_expand_incompressible_max_size_blocks() {
    assert_fastest_does_not_exceed_raw(128 * 1024);
}

fn assert_fastest_does_not_exceed_raw(len: usize) {
    let data = xorshift(len);
    let raw = compress_to_vec(data.as_slice(), CompressionLevel::Uncompressed);
    let fastest = compress_to_vec(data.as_slice(), CompressionLevel::Fastest);

    assert!(
        fastest.len() <= raw.len(),
        "fastest output should not exceed raw frame size for {len} bytes: {} > {}",
        fastest.len(),
        raw.len()
    );
}

fn xorshift(len: usize) -> Vec<u8> {
    let mut state = 0x1234_5678_9ABC_DEF0u64;
    let mut data = Vec::with_capacity(len);
    while data.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        data.extend_from_slice(&state.to_le_bytes());
    }
    data.truncate(len);
    data
}
