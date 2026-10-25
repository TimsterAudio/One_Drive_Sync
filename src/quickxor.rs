//! OneDrive's QuickXorHash, so local files can be compared with remote ones
//! without downloading them. A port of Microsoft's reference code.

use base64::Engine as _;

const WIDTH_IN_BITS: usize = 160;
const SHIFT: usize = 11;
const CELLS: usize = WIDTH_IN_BITS.div_ceil(64);

#[derive(Clone, Default)]
pub struct QuickXorHash {
    data: [u64; CELLS],
    shift_so_far: usize,
    length_so_far: u64,
}

impl QuickXorHash {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, bytes: &[u8]) {
        let cb = bytes.len();
        let mut index = self.shift_so_far / 64;
        let mut offset = self.shift_so_far % 64;
        let iterations = cb.min(WIDTH_IN_BITS);

        for i in 0..iterations {
            let is_last = index == CELLS - 1;
            let bits_in_cell = if is_last { WIDTH_IN_BITS % 64 } else { 64 };

            if offset <= bits_in_cell - 8 {
                for &b in bytes[i..].iter().step_by(WIDTH_IN_BITS) {
                    self.data[index] ^= (b as u64) << offset;
                }
            } else {
                let index2 = if is_last { 0 } else { index + 1 };
                let low = bits_in_cell - offset;
                let mut xored = 0u8;
                for &b in bytes[i..].iter().step_by(WIDTH_IN_BITS) {
                    xored ^= b;
                }
                self.data[index] ^= (xored as u64) << offset;
                self.data[index2] ^= (xored as u64) >> low;
            }

            offset += SHIFT;
            while offset >= bits_in_cell {
                index = if is_last { 0 } else { index + 1 };
                offset -= bits_in_cell;
            }
        }

        self.shift_so_far = (self.shift_so_far + SHIFT * (cb % WIDTH_IN_BITS)) % WIDTH_IN_BITS;
        self.length_so_far += cb as u64;
    }

    pub fn finish(&self) -> [u8; WIDTH_IN_BITS / 8] {
        let mut out = [0u8; WIDTH_IN_BITS / 8];
        for (i, cell) in self.data.iter().enumerate().take(CELLS - 1) {
            out[i * 8..i * 8 + 8].copy_from_slice(&cell.to_le_bytes());
        }
        let rest = out.len() - (CELLS - 1) * 8;
        out[(CELLS - 1) * 8..].copy_from_slice(&self.data[CELLS - 1].to_le_bytes()[..rest]);
        let len = self.length_so_far.to_le_bytes();
        let start = out.len() - len.len();
        for (o, l) in out[start..].iter_mut().zip(len) {
            *o ^= l;
        }
        out
    }

    /// The hash as OneDrive reports it (`file.hashes.quickXorHash`).
    pub fn finish_base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.finish())
    }
}

/// The base64 QuickXorHash of `bytes`.
pub fn hash(bytes: &[u8]) -> String {
    let mut h = QuickXorHash::new();
    h.update(bytes);
    h.finish_base64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty() {
        assert_eq!(hash(b""), "AAAAAAAAAAAAAAAAAAAAAAAAAAA=");
    }

    #[test]
    fn known_values() {
        assert_eq!(hash(b"hello world"), "aCgDG9jwBhDc4Q1yawMZAAAAAAA=");
        let data: Vec<u8> = (0..5000u32).map(|i| (i * 31 % 251) as u8).collect();
        assert_eq!(hash(&data), "Pw80W/Bnp8xB/ysb4JWctV4tDH0=");
    }

    #[test]
    fn chunked_matches_whole() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i * 31 % 251) as u8).collect();
        let whole = hash(&data);
        for chunk in [1, 7, 159, 160, 161, 1000] {
            let mut h = QuickXorHash::new();
            for c in data.chunks(chunk) {
                h.update(c);
            }
            assert_eq!(h.finish_base64(), whole, "chunk size {chunk}");
        }
    }
}
