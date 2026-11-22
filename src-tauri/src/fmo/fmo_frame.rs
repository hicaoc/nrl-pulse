//! FMO/RAW 语音帧构造与解析（发射侧构造 + 接收侧解析）。
//!
//! V4 布局：偏移 [6..10] 是发送方 UID（u32 小端，服务器身份控制按此校验，
//! 必须与 SAS 认证证书 uid 一致）；早期逆向曾误判为「16 位会话号 + 2 字节填充」。

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
    uid: u32,
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
    hdr.extend_from_slice(&uid.to_le_bytes());
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
    uid: u32,
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
        uid,
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
    uid: u32,
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
        uid,
        ts1,
        ts2,
        &blocks,
        payloads.len() as u16,
        buf_depth,
    )
}

#[derive(Debug, Clone)]
pub struct ParsedFrame {
    /// 包头 [6..10]：发送方 UID（u32 小端）
    pub uid: u32,
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
        // V4 设备把内层 5 字节同步标记从 CONST5 改成了全零（实网抓包核实，
        // 其余结构与 CONST5 格式一致），两种标记都接受；其它垃圾值仍拒绝。
        if body.len() < 5 || (body[..5] != CONST5 && body[..5] != [0u8; 5]) {
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
        uid: u32::from_le_bytes(f[6..10].try_into().ok()?),
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
        assert_eq!(p.uid, 0x1234);
        assert_eq!(p.packets.len(), 2);
        let rebuilt = build_frame(&p.callsign, p.uid, p.ts1, p.ts2, &p.packets, p.buf_depth);
        assert_eq!(rebuilt, frame);
    }

    #[test]
    fn header_uid_is_u32_at_offset_6() {
        // V4 布局：包头 [6..10] = 发送方 UID（u32 小端），服务器身份控制按此校验
        let frame = build_frame("BG9JYT", 5200, 0, 0, &[vec![0x80; 12]], 9);
        assert_eq!(&frame[6..10], &5200u32.to_le_bytes());
        // 超过 16 位的 uid 也必须完整写入（旧布局只写 u16 会话号会截断）
        let frame = build_frame("BG9JYT", 43671 + 0x1_0000, 0, 0, &[vec![0x80; 12]], 9);
        assert_eq!(parse_frame(&frame).unwrap().uid, 43671 + 0x1_0000);
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

    /// 辅助：把帧块区内的 CONST5 标记替换为指定值并重算 CRC（模拟对端设备）。
    fn rewrite_marker(frame: &mut Vec<u8>, marker: [u8; 5]) {
        let mut blocks = frame[64..].to_vec();
        let mut i = 0;
        while i + 5 <= blocks.len() {
            if blocks[i..i + 5] == CONST5 {
                blocks[i..i + 5].copy_from_slice(&marker);
                i += 5;
            } else {
                i += 1;
            }
        }
        let crc = crc32(&blocks);
        frame[36..40].copy_from_slice(&crc.to_le_bytes());
        frame[64..].copy_from_slice(&blocks);
    }

    #[test]
    fn v4_zero_marker_accepted() {
        // V4 设备回归：内层 5 字节同步标记为全零的帧也必须可解析。
        // 实网抓包（58.240.80.50，87 帧）核实：失败帧标记均为 00*5，
        // 外层块长/内层长度/Opus TOC 与 CONST5 格式完全一致。
        let packets = vec![vec![0x10; 57], vec![0x10; 63], vec![0x10; 25]];
        let mut frame = build_frame("BD4XGT", 5200, 1000, 1240, &packets, 9);
        rewrite_marker(&mut frame, [0u8; 5]);
        let p = parse_frame(&frame).expect("V4 全零标记的帧必须可解析");
        assert_eq!(p.callsign, "BD4XGT");
        assert_eq!(p.uid, 5200);
        assert_eq!(p.packets, packets);
    }

    #[test]
    fn garbage_marker_rejected() {
        // 全零以外的非法标记仍整帧拒绝
        let mut frame = build_frame("BD4XGT", 1, 0, 0, &[vec![0x10; 57]], 0);
        rewrite_marker(&mut frame, [0xAA; 5]);
        assert!(parse_frame(&frame).is_none());
    }
}
