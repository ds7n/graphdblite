/// Delta-varint encoding for sorted node ID lists.
///
/// IDs are sorted, then stored as deltas encoded with unsigned LEB128 (varint).
/// This compresses sequential IDs significantly: 1000 sequential u64s go from
/// 8KB to ~1.5KB.
/// Encode a sorted slice of u64 values using delta-varint encoding.
use crate::types::{GraphError, Result};

pub fn encode_id_list(ids: &[u64]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(ids.len() * 2);
    let mut prev = 0u64;
    for &id in ids {
        debug_assert!(id >= prev, "IDs must be sorted");
        let delta = id - prev;
        encode_varint(delta, &mut buf);
        prev = id;
    }
    buf
}

/// Decode a delta-varint encoded byte buffer back to sorted u64 values.
///
/// The buffer comes from the database file, which may be corrupt or crafted:
/// a truncated or over-long varint, or deltas whose running sum overflows
/// `u64`, return `GraphError::Serialization` instead of panicking or
/// producing wrapped ids.
pub fn decode_id_list(data: &[u8]) -> Result<Vec<u64>> {
    let mut ids = Vec::new();
    let mut pos = 0;
    let mut prev = 0u64;
    while pos < data.len() {
        let (delta, bytes_read) = decode_varint(&data[pos..])?;
        pos += bytes_read;
        prev = prev
            .checked_add(delta)
            .ok_or_else(|| corrupt_id_list("node id delta overflows u64"))?;
        ids.push(prev);
    }
    Ok(ids)
}

/// Maximum encoded length of a u64 LEB128 varint (ceil(64 / 7)).
const MAX_VARINT_BYTES: usize = 10;

fn corrupt_id_list(reason: &str) -> GraphError {
    GraphError::Serialization {
        context: "adjacency list".to_string(),
        source: format!("corrupt varint id list: {reason}"),
        hint: Some("the database file is damaged or was not written by graphdblite".to_string()),
    }
}

/// Encode a u64 as unsigned LEB128 varint.
fn encode_varint(mut value: u64, buf: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Decode an unsigned LEB128 varint, returning `(value, bytes_read)`.
/// Errors when the input is truncated (no terminating byte), longer than
/// [`MAX_VARINT_BYTES`], or encodes a value that does not fit in a u64.
fn decode_varint(data: &[u8]) -> Result<(u64, usize)> {
    let mut value: u64 = 0;
    for (i, &byte) in data.iter().take(MAX_VARINT_BYTES).enumerate() {
        let bits = (byte & 0x7F) as u64;
        let shift = 7 * i as u32;
        // The 10th byte may only carry the single remaining bit of a u64.
        if i == MAX_VARINT_BYTES - 1 && bits > 1 {
            return Err(corrupt_id_list("varint overflows u64"));
        }
        value |= bits << shift;
        if byte & 0x80 == 0 {
            return Ok((value, i + 1));
        }
    }
    if data.len() > MAX_VARINT_BYTES {
        Err(corrupt_id_list("varint longer than 10 bytes"))
    } else {
        Err(corrupt_id_list("truncated varint"))
    }
}

/// Insert an ID into a sorted list (maintains sorted order, no duplicates).
pub fn insert_into_sorted(ids: &mut Vec<u64>, id: u64) {
    match ids.binary_search(&id) {
        Ok(_) => {} // already present
        Err(pos) => ids.insert(pos, id),
    }
}

/// Remove an ID from a sorted list. Returns true if it was present.
pub fn remove_from_sorted(ids: &mut Vec<u64>, id: u64) -> bool {
    match ids.binary_search(&id) {
        Ok(pos) => {
            ids.remove(pos);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_empty() {
        let ids: Vec<u64> = vec![];
        let encoded = encode_id_list(&ids);
        assert!(encoded.is_empty());
        assert_eq!(decode_id_list(&encoded).unwrap(), ids);
    }

    #[test]
    fn roundtrip_single() {
        let ids = vec![42];
        let encoded = encode_id_list(&ids);
        assert_eq!(decode_id_list(&encoded).unwrap(), ids);
    }

    #[test]
    fn roundtrip_sequential() {
        let ids: Vec<u64> = (1..=1000).collect();
        let encoded = encode_id_list(&ids);
        // Sequential IDs should compress well — each delta is 1, which is 1 byte
        assert_eq!(encoded.len(), 1000);
        assert_eq!(decode_id_list(&encoded).unwrap(), ids);
    }

    #[test]
    fn roundtrip_sparse() {
        let ids = vec![100, 10_000, 1_000_000, u64::MAX / 2];
        let encoded = encode_id_list(&ids);
        assert_eq!(decode_id_list(&encoded).unwrap(), ids);
    }

    fn assert_corrupt(data: &[u8], reason: &str) {
        match decode_id_list(data) {
            Err(GraphError::Serialization { source, .. }) => {
                assert!(source.contains(reason), "{source}")
            }
            other => panic!("expected Serialization error ({reason}), got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_u64_max_uses_ten_bytes() {
        let ids = vec![u64::MAX];
        let encoded = encode_id_list(&ids);
        assert_eq!(encoded.len(), MAX_VARINT_BYTES);
        assert_eq!(decode_id_list(&encoded).unwrap(), ids);
    }

    #[test]
    fn decode_rejects_ten_byte_varint_overflowing_u64() {
        // u64::MAX's last byte is 0x01; 0x02 sets bit 64.
        let mut bad = vec![0xFF; 9];
        bad.push(0x02);
        assert_corrupt(&bad, "overflows u64");
    }

    #[test]
    fn decode_rejects_eleven_byte_varint() {
        // Boundary: 10 continuation bytes then a terminator (11 bytes).
        let mut bad = vec![0x80; 10];
        bad.push(0x00);
        assert_corrupt(&bad, "longer than 10 bytes");
        assert_corrupt(&[0xFF; 20], "overflows u64");
    }

    #[test]
    fn decode_rejects_truncated_varint() {
        assert_corrupt(&[0x01, 0x80], "truncated");
        assert_corrupt(&[0xFF; 10], "overflows u64");
    }

    #[test]
    fn decode_rejects_delta_sum_overflow() {
        let mut data = encode_id_list(&[u64::MAX]);
        data.push(0x01); // delta 1 on top of u64::MAX
        assert_corrupt(&data, "delta overflows u64");
    }

    #[test]
    fn insert_and_remove() {
        let mut ids = vec![1, 3, 5, 7];
        insert_into_sorted(&mut ids, 4);
        assert_eq!(ids, vec![1, 3, 4, 5, 7]);
        insert_into_sorted(&mut ids, 4); // duplicate — no-op
        assert_eq!(ids, vec![1, 3, 4, 5, 7]);
        assert!(remove_from_sorted(&mut ids, 4));
        assert_eq!(ids, vec![1, 3, 5, 7]);
        assert!(!remove_from_sorted(&mut ids, 4)); // already gone
    }
}
