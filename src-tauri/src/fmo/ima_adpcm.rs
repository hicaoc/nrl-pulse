//! IMA/DVI ADPCM 编解码（FMO/RAW 0x02 块）。

const INDEX_TABLE: [i32; 16] = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8];

const STEP_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];

fn clamp(v: i32, lo: i32, hi: i32) -> i32 {
    v.max(lo).min(hi)
}

/// 解码一个 320B ADPCM 块 → 640 样本 s16le PCM（8kHz mono）。
pub fn decode_block(payload: &[u8], mut valprev: i32, mut index: i32) -> Vec<u8> {
    index = clamp(index, 0, 88);
    let mut out = Vec::with_capacity(payload.len() * 2);
    for &byte in payload {
        for nib in [byte >> 4, byte & 0x0F] {
            let step = STEP_TABLE[index as usize];
            let mut diff = step >> 3;
            if nib & 4 != 0 {
                diff += step;
            }
            if nib & 2 != 0 {
                diff += step >> 1;
            }
            if nib & 1 != 0 {
                diff += step >> 2;
            }
            valprev = if nib & 8 != 0 {
                valprev - diff
            } else {
                valprev + diff
            };
            valprev = clamp(valprev, -32768, 32767);
            index = clamp(index + INDEX_TABLE[nib as usize], 0, 88);
            out.extend_from_slice(&(valprev as i16).to_le_bytes());
        }
    }
    out
}

/// 8kHz s16le → 48kHz s16le（线性插值，6× 上采样）。
pub fn resample_8k_to_48k(pcm: &[u8]) -> Vec<u8> {
    let n = pcm.len() / 2;
    if n < 2 {
        return Vec::new();
    }
    let samples: Vec<i32> = pcm
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as i32)
        .collect();
    let mut out = Vec::with_capacity(n * 6 * 2);
    let mut prev = samples[0];
    for i in 1..n {
        let cur = samples[i];
        let d = cur - prev;
        for k in 0..6 {
            out.extend_from_slice(&((prev + d * k / 6) as i16).to_le_bytes());
        }
        prev = cur;
    }
    for _ in 0..6 {
        out.extend_from_slice(&(samples[n - 1] as i16).to_le_bytes());
    }
    out
}

/// 640 样本 s16le(8kHz) → (320B ADPCM, 新 valprev, 新 index)。
pub fn encode_block(pcm: &[u8], mut valprev: i32, mut index: i32) -> (Vec<u8>, i32, i32) {
    index = clamp(index, 0, 88);
    let n = pcm.len() / 2;
    let mut nibbles: Vec<u8> = Vec::with_capacity(n);
    for chunk in pcm.chunks_exact(2) {
        let s = i16::from_le_bytes([chunk[0], chunk[1]]) as i32;
        let step = STEP_TABLE[index as usize];
        let mut diff = s - valprev;
        let mut nibble = 0u8;
        if diff < 0 {
            nibble = 8;
            diff = -diff;
        }
        let mut vpdiff = step >> 3;
        if diff >= step {
            nibble |= 4;
            diff -= step;
            vpdiff += step;
        }
        if diff >= step >> 1 {
            nibble |= 2;
            diff -= step >> 1;
            vpdiff += step >> 1;
        }
        if diff >= step >> 2 {
            nibble |= 1;
            vpdiff += step >> 2;
        }
        valprev = if nibble & 8 != 0 {
            valprev - vpdiff
        } else {
            valprev + vpdiff
        };
        valprev = clamp(valprev, -32768, 32767);
        index = clamp(index + INDEX_TABLE[nibble as usize], 0, 88);
        nibbles.push(nibble);
    }
    let mut out = Vec::with_capacity(nibbles.len() / 2);
    let mut i = 0;
    while i + 1 < nibbles.len() {
        out.push((nibbles[i] << 4) | nibbles[i + 1]);
        i += 2;
    }
    (out, valprev, index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut pcm = Vec::with_capacity(1280);
        for i in 0..640i32 {
            let v = ((i as f32) * 0.1).sin() * 8000.0;
            pcm.extend_from_slice(&(v as i16).to_le_bytes());
        }
        let (encoded, vp, idx) = encode_block(&pcm, 0, 0);
        assert_eq!(encoded.len(), 320);
        let decoded = decode_block(&encoded, 0, 0);
        assert_eq!(decoded.len(), 1280);
        let d0 = i16::from_le_bytes([decoded[0], decoded[1]]) as i32;
        let o0 = i16::from_le_bytes([pcm[0], pcm[1]]) as i32;
        assert!((d0 - o0).abs() < 64, "first sample diff {} vs {}", d0, o0);
    }

    #[test]
    fn resample_len() {
        let pcm = vec![0u8; 160 * 2];
        let out = resample_8k_to_48k(&pcm);
        assert_eq!(out.len(), 160 * 6 * 2);
    }
}
