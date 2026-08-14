//! FMO/RAW 音频编解码：Opus（SILK NB 8kHz）与 IMA ADPCM，及 PTT 发射通道。
//!
//! 与 sim-rust 的差异：本应用音频管线统一为 8kHz s16le mono，
//! 因此 opus 解码/编码均使用 8000Hz，ADPCM 无需重采样，直接复用 ima_adpcm。

use crate::fmo::fmo_frame;
use crate::fmo::ima_adpcm;
use crate::fmo::mqtt_client::FmoMqttClient;
use opus::{Application, Decoder, Encoder};
use std::sync::Arc;

pub struct OpusCodec {
    pub decoder: Decoder,
    pub encoder: Encoder,
}

impl OpusCodec {
    pub fn new() -> Result<Self, String> {
        // FMO/RAW 语音为 SILK NB 8kHz；解码器以 8000Hz 创建即得 8k PCM。
        let decoder = Decoder::new(8000, opus::Channels::Mono).map_err(|e| e.to_string())?;
        let encoder = Encoder::new(8000, opus::Channels::Mono, Application::Voip)
            .map_err(|e| e.to_string())?;
        Ok(Self { decoder, encoder })
    }

    /// 解码一帧 Opus 包 → PCM（8kHz s16le mono）。40ms@8k = 320 样本。
    pub fn decode_frame(&mut self, packet: &[u8]) -> Result<Vec<i16>, String> {
        let mut pcm = [0i16; 320];
        let n = self
            .decoder
            .decode(packet, &mut pcm, false)
            .map_err(|e| e.to_string())?;
        Ok(pcm[..n].to_vec())
    }

    /// 编码 40ms PCM（8kHz 320 样本）→ Opus 包（SILK NB）。
    pub fn encode_frame(&mut self, pcm: &[i16]) -> Result<Vec<u8>, String> {
        let mut out = [0u8; 1500];
        let n = self
            .encoder
            .encode(pcm, &mut out)
            .map_err(|e| e.to_string())?;
        Ok(out[..n].to_vec())
    }
}

pub type OnPcmFn = Box<dyn Fn(&[i16]) + Send + Sync>;

/// 接收侧音频：串行解码 FMO/RAW 帧内 Opus/ADPCM 块。
pub struct RxAudio {
    pub codec: Arc<std::sync::Mutex<OpusCodec>>,
    pub on_pcm: std::sync::Mutex<Option<OnPcmFn>>,
}

impl RxAudio {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            codec: Arc::new(std::sync::Mutex::new(OpusCodec::new()?)),
            on_pcm: std::sync::Mutex::new(None),
        })
    }

    pub fn feed_packets(&self, packets: &[Vec<u8>]) -> Result<(), String> {
        if packets.is_empty() {
            return Ok(());
        }
        let mut codec = self.codec.lock().map_err(|e| e.to_string())?;
        for pkt in packets {
            let pcm = codec.decode_frame(pkt)?;
            if let Ok(guard) = self.on_pcm.lock() {
                if let Some(cb) = guard.as_ref() {
                    cb(&pcm);
                }
            }
        }
        Ok(())
    }

    pub fn feed_adpcm(&self, blocks: &[(i16, u8, Vec<u8>)]) {
        for (vp, idx, data) in blocks {
            let pcm8 = ima_adpcm::decode_block(data, *vp as i32, *idx as i32);
            let pcm: Vec<i16> = pcm8
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect();
            if let Ok(guard) = self.on_pcm.lock() {
                if let Some(cb) = guard.as_ref() {
                    cb(&pcm);
                }
            }
        }
    }
}

// ---------------------------------------------------------------- 发射通道

pub const PACKETS_PER_FRAME: usize = 6; // opus：每帧 6 个 40ms 包 ≈ 240ms
pub const BLOCKS_PER_FRAME: usize = 3; // adpcm：每帧 3 块 × 80ms = 240ms
pub const FIRST_FRAMES_BUF0: usize = 3; // 连发前 3 帧 buf_depth=0
pub const ADPCM_PARAM: u8 = 0x00;
pub const ADPCM_MARKER: u8 = 0xAA;

pub struct TxSession {
    pub callsign: String,
    pub mode: String, // "opus" | "adpcm"
    pub session: u16,
    pub ts1: u32,
    pub packets_sent: Arc<std::sync::Mutex<usize>>,
    pub frames_sent: Arc<std::sync::Mutex<usize>>,
    pub mqtt: Arc<FmoMqttClient>,
    /// 累计发射帧计数器（FMO 独立统计，跨会话累计）
    pub total_tx_counter: Option<Arc<std::sync::atomic::AtomicU64>>,
    // adpcm 状态
    pub adpcm_state: Arc<std::sync::Mutex<(i32, i32)>>,
    pub adpcm_seq: Arc<std::sync::Mutex<u8>>,
    pub pcm_buf: Arc<std::sync::Mutex<Vec<i16>>>,
    pub pending: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    // opus 状态
    pub codec: std::sync::Mutex<OpusCodec>,
    pub closed: Arc<std::sync::Mutex<bool>>,
}

impl TxSession {
    pub fn new(
        mqtt: Arc<FmoMqttClient>,
        callsign: &str,
        mode: &str,
        total_tx_counter: Option<Arc<std::sync::atomic::AtomicU64>>,
    ) -> Result<Self, String> {
        let session = (chrono::Utc::now().timestamp_micros() & 0xFFFF) as u16;
        let session = if session == 0 { 1 } else { session };
        let ts1 = (chrono::Utc::now().timestamp_millis() & 0xFFFFFFFF) as u32;
        Ok(Self {
            callsign: callsign.to_string(),
            mode: if mode == "adpcm" {
                "adpcm".into()
            } else {
                "opus".into()
            },
            session,
            ts1,
            packets_sent: Arc::new(std::sync::Mutex::new(0)),
            frames_sent: Arc::new(std::sync::Mutex::new(0)),
            mqtt,
            total_tx_counter,
            adpcm_state: Arc::new(std::sync::Mutex::new((0, 0))),
            adpcm_seq: Arc::new(std::sync::Mutex::new(0)),
            pcm_buf: Arc::new(std::sync::Mutex::new(Vec::new())),
            pending: Arc::new(std::sync::Mutex::new(Vec::new())),
            codec: std::sync::Mutex::new(OpusCodec::new()?),
            closed: Arc::new(std::sync::Mutex::new(false)),
        })
    }

    async fn encode_and_queue(&self, packets: Vec<Vec<u8>>) {
        let ms_per = if self.mode == "adpcm" { 80 } else { 40 };
        let pkt_len = packets.len();
        if pkt_len == 0 {
            return;
        }
        let (ts2, buf_depth) = {
            let mut ps = self.packets_sent.lock().unwrap();
            let ts2 = (self.ts1 as u64 + (*ps as u64) * ms_per as u64) & 0xFFFFFFFF;
            let mut fs = self.frames_sent.lock().unwrap();
            let buf_depth = if *fs < FIRST_FRAMES_BUF0 { 0u8 } else { 9u8 };
            *ps += pkt_len;
            *fs += 1;
            (ts2, buf_depth)
        };
        let frame = if self.mode == "adpcm" {
            fmo_frame::build_frame_adpcm(
                &self.callsign,
                self.session,
                self.ts1,
                ts2 as u32,
                &packets,
                buf_depth,
            )
        } else {
            fmo_frame::build_frame(
                &self.callsign,
                self.session,
                self.ts1,
                ts2 as u32,
                &packets,
                buf_depth,
            )
        };
        let _ = self.mqtt.publish("FMO/RAW", frame, 0).await;
        if let Some(counter) = &self.total_tx_counter {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// feed_pcm 入口：PCM 8kHz s16le mono（Vec<i16>）。
    pub async fn feed_pcm(&self, pcm: &[i16]) {
        if *self.closed.lock().unwrap() {
            return;
        }
        if self.mode == "adpcm" {
            self.feed_adpcm(pcm).await;
        } else {
            self.feed_opus(pcm).await;
        }
    }

    async fn feed_adpcm(&self, pcm: &[i16]) {
        let chunks: Vec<Vec<i16>> = {
            let mut buf = self.pcm_buf.lock().unwrap();
            buf.extend_from_slice(pcm);
            let mut chunks = Vec::new();
            while buf.len() >= 640 {
                let chunk: Vec<i16> = buf.drain(..640).collect();
                chunks.push(chunk);
            }
            chunks
        };
        for chunk in chunks {
            self.encode_block(&chunk);
            let ready = self.pending.lock().unwrap().len() >= BLOCKS_PER_FRAME;
            if ready {
                let blocks = std::mem::take(&mut *self.pending.lock().unwrap());
                self.encode_and_queue(blocks).await;
            }
        }
    }

    fn encode_block(&self, pcm640: &[i16]) {
        let (vp, idx) = *self.adpcm_state.lock().unwrap();
        let (payload, vp2, idx2) = ima_adpcm::encode_block(&to_bytes(pcm640), vp, idx);
        *self.adpcm_state.lock().unwrap() = (vp2, idx2);
        let mut seq = self.adpcm_seq.lock().unwrap();
        let mut hdr = vec![*seq & 0xFF, ADPCM_PARAM];
        hdr.extend_from_slice(&(vp as i16).to_le_bytes());
        hdr.push(idx as u8);
        hdr.extend_from_slice(&[0x00, 0x40, ADPCM_MARKER]);
        *seq = seq.wrapping_add(1);
        let mut blk = hdr;
        blk.extend_from_slice(&payload);
        self.pending.lock().unwrap().push(blk);
    }

    async fn feed_opus(&self, pcm: &[i16]) {
        const FRAME_SAMPLES: usize = 320;
        let frames: Vec<Vec<i16>> = {
            let mut buf = self.pcm_buf.lock().unwrap();
            buf.extend_from_slice(pcm);
            let mut frames = Vec::new();
            while buf.len() >= FRAME_SAMPLES {
                let chunk: Vec<i16> = buf.drain(..FRAME_SAMPLES).collect();
                frames.push(chunk);
            }
            frames
        };
        for samples in frames {
            let pkt = self.codec.lock().unwrap().encode_frame(&samples);
            if let Ok(pkt) = pkt {
                let ready = {
                    let mut pending = self.pending.lock().unwrap();
                    pending.push(pkt);
                    pending.len() >= PACKETS_PER_FRAME
                };
                if ready {
                    let pkts = std::mem::take(&mut *self.pending.lock().unwrap());
                    self.encode_and_queue(pkts).await;
                }
            }
        }
    }

    /// 结束：flush 尾部不足一块的 PCM。
    pub async fn stop(&self) {
        {
            let mut closed = self.closed.lock().unwrap();
            if *closed {
                return;
            }
            *closed = true;
        }
        if self.mode == "adpcm" {
            let buf = self.pcm_buf.lock().unwrap().clone();
            if buf.len() >= 2 {
                let mut padded = buf.clone();
                while padded.len() < 640 {
                    let last = *padded.last().unwrap_or(&0);
                    padded.push(last);
                }
                self.encode_block(&padded[..640]);
                let blocks = std::mem::take(&mut *self.pending.lock().unwrap());
                if !blocks.is_empty() {
                    self.encode_and_queue(blocks).await;
                }
            }
        }
    }
}

fn to_bytes(pcm: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}
