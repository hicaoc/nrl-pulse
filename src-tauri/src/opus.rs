//! NRL 协议 type=8 Opus 语音编解码。
//!
//! 规格（由用户提供）：
//! - Sample rate: 16kHz
//! - Channels: 1
//! - Frame size: 20ms (320 samples @16k)
//! - Bitrate: 32–40 kbps (Default VBR)
//! - Application: OPUS_APPLICATION_VOIP
//! - Complexity: 10
//!
//! 本应用音频管线统一为 8kHz s16le mono，因此在编码前将 8k → 16k 上采样，
//! 解码后将 16k → 8k 下采样，再交给既有回放链路。

use opus::{Application, Decoder, Encoder};

pub const NRL_OPUS_SAMPLE_RATE: u32 = 16_000;
pub const NRL_OPUS_FRAME_SAMPLES: usize = 320; // 20ms @ 16k

/// 16k Opus 编码器（VOIP / VBR 32-40kbps / complexity 10）。
pub struct NrlOpusEncoder {
    encoder: Encoder,
    /// 8k 输入 PCM 缓冲（i16）
    in_buf: Vec<i16>,
    /// 8k → 16k 线性插值上采样后的输出缓冲
    out_buf: Vec<i16>,
}

impl NrlOpusEncoder {
    pub fn new() -> Result<Self, String> {
        let mut encoder = Encoder::new(
            NRL_OPUS_SAMPLE_RATE,
            opus::Channels::Mono,
            Application::Voip,
        )
        .map_err(|e| e.to_string())?;
        encoder
            .set_bitrate(opus::Bitrate::Bits(32_000))
            .map_err(|e| e.to_string())?;
        encoder.set_vbr(true).map_err(|e| e.to_string())?;
        encoder.set_complexity(10).map_err(|e| e.to_string())?;
        Ok(Self {
            encoder,
            in_buf: Vec::new(),
            out_buf: Vec::new(),
        })
    }

    /// 喂入 8k PCM，返回满 20ms(320@16k) 的 Opus 包。
    pub fn feed(&mut self, pcm8: &[i16]) -> Result<Vec<Vec<u8>>, String> {
        self.in_buf.extend_from_slice(pcm8);
        // 8k → 16k 线性插值（两倍率）
        let upsample: Vec<i16> = resample_8k_to_16k(&self.in_buf);
        self.in_buf.clear();
        self.out_buf.extend_from_slice(&upsample);

        let mut packets = Vec::new();
        let mut out = [0u8; 1500];
        while self.out_buf.len() >= NRL_OPUS_FRAME_SAMPLES {
            let frame: Vec<i16> = self.out_buf.drain(..NRL_OPUS_FRAME_SAMPLES).collect();
            let n = self
                .encoder
                .encode(&frame, &mut out)
                .map_err(|e| e.to_string())?;
            packets.push(out[..n].to_vec());
        }
        Ok(packets)
    }

    /// 清空跨帧缓冲（发射停止时调用）。
    pub fn reset(&mut self) {
        self.in_buf.clear();
        self.out_buf.clear();
    }
}

/// 16k Opus 解码器。
pub struct NrlOpusDecoder {
    decoder: Decoder,
}

impl NrlOpusDecoder {
    pub fn new() -> Result<Self, String> {
        let decoder =
            Decoder::new(NRL_OPUS_SAMPLE_RATE, opus::Channels::Mono).map_err(|e| e.to_string())?;
        Ok(Self { decoder })
    }

    /// 解码一个 Opus 包 → 16k PCM（i16）。
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<i16>, String> {
        let mut pcm = [0i16; NRL_OPUS_FRAME_SAMPLES];
        let n = self
            .decoder
            .decode(packet, &mut pcm, false)
            .map_err(|e| e.to_string())?;
        Ok(pcm[..n].to_vec())
    }
}

/// 8k → 16k 线性插值上采样。
pub fn resample_8k_to_16k(input: &[i16]) -> Vec<i16> {
    if input.is_empty() {
        return Vec::new();
    }
    if input.len() == 1 {
        return vec![input[0]; 2];
    }
    let mut out = Vec::with_capacity(input.len() * 2);
    for i in 0..input.len() - 1 {
        let a = input[i] as i32;
        let b = input[i + 1] as i32;
        out.push(a as i16);
        out.push(((a + b) / 2) as i16);
    }
    let last = *input.last().unwrap_or(&0);
    out.push(last);
    out.push(last);
    out
}

/// 16k → 8k 平均下采样。
pub fn resample_16k_to_8k(input: &[i16]) -> Vec<i16> {
    let mut out = Vec::with_capacity(input.len() / 2);
    let mut i = 0;
    while i + 1 < input.len() {
        let avg = ((input[i] as i32 + input[i + 1] as i32) / 2) as i16;
        out.push(avg);
        i += 2;
    }
    if input.len() % 2 == 1 {
        out.push(*input.last().unwrap_or(&0));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_sizes() {
        let pcm8: Vec<i16> = (0..160).collect();
        let up = resample_8k_to_16k(&pcm8);
        assert_eq!(up.len(), 320);
        let down = resample_16k_to_8k(&up);
        assert_eq!(down.len(), 160);
        // 下采样恢复值应与原值接近（线性插值）
        assert!((down[0] - pcm8[0]).abs() <= 1);
    }

    #[test]
    fn encoder_roundtrip() {
        let mut enc = NrlOpusEncoder::new().unwrap();
        let pcm8: Vec<i16> = (0..320)
            .map(|i| ((i as f32 * 0.2).sin() * 8000.0) as i16)
            .collect();
        let packets = enc.feed(&pcm8).unwrap();
        // 320 样本 @8k = 40ms → 2 个 20ms 包 @16k
        assert_eq!(packets.len(), 2);
        let mut dec = NrlOpusDecoder::new().unwrap();
        let pcm16 = dec.decode(&packets[0]).unwrap();
        assert_eq!(pcm16.len(), NRL_OPUS_FRAME_SAMPLES);
    }
}
