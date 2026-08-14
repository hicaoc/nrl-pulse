//! FMO/RAW 语音帧构造与解析（发射侧构造 + 接收侧解析）。

pub const CONST5: [u8; 5] = [0x3d, 0x14, 0x00, 0xe0, 0x3d];

/// CRC32（zlib/png 语义，python zlib.crc32 兼容）。
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB88320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn pack(
    callsign: &str,
    session: u16,
    ts1: u32,
    ts2: u32,
    blocks: &[u8],
    nblocks: u16,
    buf_depth: u8,
) -> Vec<u8> {
    let total = 64 + blocks.len() as u32;
    let mut cs = [0u8; 6];
    let bytes = callsign.to_uppercase().into_bytes();
    for (i, b) in bytes.iter().take(6).enumerate() {
        cs[i] = *b;
    }
    let mut hdr = Vec::with_capacity(64);
    hdr.extend_from_slice(&[0x01, 0, 0, 0, 0, 0]);
    hdr.extend_from_slice(&session.to_le_bytes());
    hdr.extend_from_slice(&[0, 0]);
    hdr.extend_from_slice(&cs);
    hdr.extend_from_slice(&[0; 6]);
    hdr.extend_from_slice(&ts1.to_le_bytes());
    hdr.extend_from_slice(&ts2.to_le_bytes());
    hdr.extend_from_slice(&total.to_le_bytes());
    hdr.extend_from_slice(&nblocks.to_le_bytes());
    hdr.extend_from_slice(&crc32(blocks).to_le_bytes());
    hdr.push(buf_depth);
    hdr.extend_from_slice(&[0xbf, 0x01, 0x00]);
    hdr.extend_from_slice(&[0; 20]);
    debug_assert_eq!(hdr.len(), 64);
    hdr.extend_from_slice(blocks);
    hdr
}

fn make_block(idx: u8, inner_type: u8, payload: &[u8]) -> Vec<u8> {
    let l = (3 + 5 + payload.len()) as u16;
    let mut inner = Vec::with_capacity(8 + l as usize);
    inner.push(inner_type);
    inner.extend_from_slice(&l.to_le_bytes());
    inner.extend_from_slice(&CONST5);
    inner.extend_from_slice(payload);
    let block_len = (8 + inner.len()) as u16;
    let mut block = Vec::with_capacity(4 + inner.len());
    block.push(idx);
    block.push(0x00);
    block.extend_from_slice(&block_len.to_le_bytes());
    block.extend_from_slice(&[0; 4]);
    block.extend_from_slice(&inner);
    block
}

/// 把若干 Opus 包（SILK NB 40ms）打包成一帧 FMO/RAW（老格式 0x01 块）。
pub fn build_frame(
    callsign: &str,
    session: u16,
    ts1: u32,
    ts2: u32,
    opus_packets: &[Vec<u8>],
    buf_depth: u8,
) -> Vec<u8> {
    let mut blocks = Vec::new();
    for (i, pkt) in opus_packets.iter().enumerate() {
        blocks.extend_from_slice(&make_block((i + 1) as u8 & 0xFF, 0x01, pkt));
    }
    pack(
        callsign,
        session,
        ts1,
        ts2,
        &blocks,
        opus_packets.len() as u16,
        buf_depth,
    )
}

/// 把若干 IMA ADPCM 块（328B：8B 头 + 320B 数据）打包成一帧（新格式 0x02 块）。
pub fn build_frame_adpcm(
    callsign: &str,
    session: u16,
    ts1: u32,
    ts2: u32,
    payloads: &[Vec<u8>],
    buf_depth: u8,
) -> Vec<u8> {
    let mut blocks = Vec::new();
    for (i, pl) in payloads.iter().enumerate() {
        blocks.extend_from_slice(&make_block((i + 1) as u8 & 0xFF, 0x02, pl));
    }
    pack(
        callsign,
        session,
        ts1,
        ts2,
        &blocks,
        payloads.len() as u16,
        buf_depth,
    )
}

#[derive(Debug, Clone)]
pub struct ParsedFrame {
    pub session: u16,
    pub callsign: String,
    pub ts1: u32,
    pub ts2: u32,
    pub block_count: u16,
    pub buf_depth: u8,
    pub packets: Vec<Vec<u8>>,
    /// IMA ADPCM 块：[(valprev s16, index u8, 320B payload)]
    pub adpcm: Vec<(i16, u8, Vec<u8>)>,
}

/// 解析一帧。校验失败返回 None。
pub fn parse_frame(f: &[u8]) -> Option<ParsedFrame> {
    if f.len() < 64 || f[0] != 1 {
        return None;
    }
    let total = u32::from_le_bytes(f[30..34].try_into().ok()?) as usize;
    if total != f.len() {
        return None;
    }
    let crc = u32::from_le_bytes(f[36..40].try_into().ok()?);
    if crc32(&f[64..]) != crc {
        return None;
    }
    let mut packets: Vec<Vec<u8>> = Vec::new();
    let mut adpcm: Vec<(i16, u8, Vec<u8>)> = Vec::new();
    let mut pos = 64;
    while pos + 4 <= f.len() {
        let blen = (f[pos + 2] as usize) | ((f[pos + 3] as usize) << 8);
        if blen < 12 || pos + blen > f.len() {
            return None;
        }
        let inner = &f[pos + 8..pos + blen];
        if inner.is_empty() {
            return None;
        }
        let ilen = (inner[1] as usize) | ((inner[2] as usize) << 8);
        let body_end = (3 + ilen).min(inner.len());
        let body = &inner[3..body_end];
        if body.len() < 5 || body[..5] != CONST5 {
            return None;
        }
        let pkt = &body[5..];
        if inner[0] == 0x01 {
            if !pkt.is_empty() {
                packets.push(pkt.to_vec());
            }
        } else if inner[0] == 0x02 {
            if pkt.len() == 328 {
                let vp = i16::from_le_bytes([pkt[2], pkt[3]]);
                adpcm.push((vp, pkt[4], pkt[8..328].to_vec()));
            }
        }
        pos += blen;
    }
    Some(ParsedFrame {
        session: u16::from_le_bytes([f[6], f[7]]),
        callsign: String::from_utf8_lossy(&f[10..16])
            .trim_end_matches('\x00')
            .to_string(),
        ts1: u32::from_le_bytes(f[22..26].try_into().ok()?),
        ts2: u32::from_le_bytes(f[26..30].try_into().ok()?),
        block_count: u16::from_le_bytes(f[34..36].try_into().ok()?),
        buf_depth: f[40],
        packets,
        adpcm,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_zlib() {
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
    }

    #[test]
    fn roundtrip_opus() {
        let frame = build_frame(
            "BG9JYT",
            0x1234,
            1000,
            1010,
            &[vec![0x80; 12], vec![0x81; 20]],
            9,
        );
        let p = parse_frame(&frame).unwrap();
        assert_eq!(p.callsign, "BG9JYT");
        assert_eq!(p.session, 0x1234);
        assert_eq!(p.packets.len(), 2);
        let rebuilt = build_frame(
            &p.callsign,
            p.session,
            p.ts1,
            p.ts2,
            &p.packets,
            p.buf_depth,
        );
        assert_eq!(rebuilt, frame);
    }

    #[test]
    fn roundtrip_adpcm() {
        let payloads = vec![vec![0xAA; 328], vec![0xBB; 328], vec![0xCC; 328]];
        let frame = build_frame_adpcm("BD4VKI", 7, 2000, 2030, &payloads, 0);
        let p = parse_frame(&frame).unwrap();
        assert_eq!(p.callsign, "BD4VKI");
        assert_eq!(p.adpcm.len(), 3);
        assert_eq!(p.packets.len(), 0);
        assert_eq!(p.block_count, 3);
    }

    #[test]
    fn reject_bad() {
        assert!(parse_frame(&[0u8; 10]).is_none());
        let mut frame = build_frame("TEST", 1, 0, 0, &[vec![0x90; 30]], 9);
        frame[10] = b'X';
        frame[36..40].copy_from_slice(&0u32.to_le_bytes());
        assert!(parse_frame(&frame).is_none());
    }
}
