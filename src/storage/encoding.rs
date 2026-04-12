/// Delta-varint encoding for sorted node ID lists.
///
/// IDs are sorted, then stored as deltas encoded with unsigned LEB128 (varint).
/// This compresses sequential IDs significantly: 1000 sequential u64s go from
/// 8KB to ~1.5KB.

/// Encode a sorted slice of u64 values using delta-varint encoding.
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
pub fn decode_id_list(data: &[u8]) -> Vec<u64> {
    let mut ids = Vec::new();
    let mut pos = 0;
    let mut prev = 0u64;
    while pos < data.len() {
        let (delta, bytes_read) = decode_varint(&data[pos..]);
        pos += bytes_read;
        prev += delta;
        ids.push(prev);
    }
    ids
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

/// Decode an unsigned LEB128 varint. Returns (value, bytes_consumed).
fn decode_varint(data: &[u8]) -> (u64, usize) {
    let mut value: u64 = 0;
    let mut shift = 0;
    for (i, &byte) in data.iter().enumerate() {
        value |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return (value, i + 1);
        }
        shift += 7;
    }
    (value, data.len())
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
        assert_eq!(decode_id_list(&encoded), ids);
    }

    #[test]
    fn roundtrip_single() {
        let ids = vec![42];
        let encoded = encode_id_list(&ids);
        assert_eq!(decode_id_list(&encoded), ids);
    }

    #[test]
    fn roundtrip_sequential() {
        let ids: Vec<u64> = (1..=1000).collect();
        let encoded = encode_id_list(&ids);
        // Sequential IDs should compress well — each delta is 1, which is 1 byte
        assert_eq!(encoded.len(), 1000);
        assert_eq!(decode_id_list(&encoded), ids);
    }

    #[test]
    fn roundtrip_sparse() {
        let ids = vec![100, 10_000, 1_000_000, u64::MAX / 2];
        let encoded = encode_id_list(&ids);
        assert_eq!(decode_id_list(&encoded), ids);
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
