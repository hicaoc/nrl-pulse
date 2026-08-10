use audioadapter_buffers::direct::SequentialSliceOfVecs;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig, SupportedStreamConfig};
use rubato::audioadapter::AdapterIterators;
use rubato::{Fft, FixedSync, Resampler};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

#[cfg(target_os = "macos")]
use crate::audio_aec_mac::AecCapture;
#[cfg(target_os = "windows")]
use crate::audio_aec_win::AecCapture;
use crate::models::DeviceSettings;

const TARGET_SAMPLE_RATE: u32 = 8_000;
const VOICE_FRAME_SAMPLES: usize = 160;
// 回放 ring buffer 最多缓存 500ms 的输入样本，超出丢弃以防积压延迟
const MAX_RING_SAMPLES: usize = TARGET_SAMPLE_RATE as usize / 2;

/// 接收通道标识：NRL / FMO 各自一条独立输出流，混音交给操作系统
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RxChannel {
    Nrl,
    Fmo,
}

/// 单通道播放状态：独立 ring + jitter 起播 + 独立静音开关
struct PlaybackState {
    ring: VecDeque<i16>,
    playing: bool,
    monitoring: Arc<AtomicBool>,
    /// jitter buffer 目标深度（8kHz 输入域样本数）：攒够才开始播
    jitter_target: usize,
    /// 设备采样率 ≠ 8kHz 时的重采样器
    resampler: Option<Fft<f32>>,
    /// 重采样后的待输出样本（设备采样率 f32）
    out_buf: VecDeque<f32>,
}

impl PlaybackState {
    fn new(output_rate: u32, monitoring: Arc<AtomicBool>) -> Result<Self, String> {
        let resampler = if output_rate == TARGET_SAMPLE_RATE {
            eprintln!(
                "[Audio] Output: device supports {} Hz directly, using passthrough",
                output_rate
            );
            None
        } else {
            eprintln!(
                "[Audio] Output: device {} Hz, creating resampler {} -> {} Hz",
                output_rate, TARGET_SAMPLE_RATE, output_rate
            );
            Some(
                Fft::<f32>::new(
                    TARGET_SAMPLE_RATE as usize,
                    output_rate as usize,
                    VOICE_FRAME_SAMPLES,
                    1,
                    1,
                    FixedSync::Both,
                )
                .map_err(|e| format!("failed to create resampler: {e}"))?,
            )
        };

        Ok(Self {
            ring: VecDeque::with_capacity(MAX_RING_SAMPLES * 2),
            playing: false,
            monitoring,
            // 200ms 目标深度：FMO 按 240ms 一帧突发到达，匀速播放需要缓冲吸收抖动
            jitter_target: TARGET_SAMPLE_RATE as usize / 5,
            resampler,
            out_buf: VecDeque::with_capacity(MAX_RING_SAMPLES * 4),
        })
    }

    fn enqueue(&mut self, pcm: &[i16]) {
        self.ring.extend(pcm.iter().copied());
        // 防止积压：超过 500ms 的数据直接丢弃头部
        while self.ring.len() > MAX_RING_SAMPLES {
            self.ring.pop_front();
        }
    }

    fn next_batch(&mut self, output_frames: usize) -> Vec<f32> {
        if !self.monitoring.load(Ordering::Relaxed) {
            return vec![0.0; output_frames];
        }

        // jitter 起播判定：攒到目标深度才开播，欠载抽空后自动回缓冲
        if !self.playing && self.ring.len() >= self.jitter_target {
            self.playing = true;
        }

        // 8kHz 直通
        if self.resampler.is_none() {
            let mut output = Vec::with_capacity(output_frames);
            for _ in 0..output_frames {
                output.push(self.pop_f32());
            }
            return output;
        }

        // 重采样：8kHz 域按 chunk 取样 → 重采样到设备采样率
        let PlaybackState {
            ring,
            playing,
            resampler,
            out_buf,
            ..
        } = self;
        let Some(resampler) = resampler.as_mut() else {
            return vec![0.0; output_frames];
        };
        while out_buf.len() < output_frames {
            let chunk_size = resampler.input_frames_next();
            let mut chunk = Vec::with_capacity(chunk_size);
            for _ in 0..chunk_size {
                chunk.push(pop_from(ring, playing));
            }
            let input = vec![chunk];
            let Ok(adapter) = SequentialSliceOfVecs::new(&input, 1, chunk_size) else {
                break;
            };
            let Ok(interleaved) = resampler.process(&adapter, 0, None) else {
                break;
            };
            if let Some(iter) = interleaved.iter_channel(0) {
                for s in iter {
                    out_buf.push_back(s);
                }
            }
        }

        let mut output = Vec::with_capacity(output_frames);
        for _ in 0..output_frames {
            output.push(out_buf.pop_front().unwrap_or(0.0));
        }
        output
    }

    fn pop_f32(&mut self) -> f32 {
        pop_from(&mut self.ring, &mut self.playing)
    }
}

/// 播放状态下取一个样本转 f32；抽空则退出播放状态（重新缓冲），返回静音
#[inline]
fn pop_from(ring: &mut VecDeque<i16>, playing: &mut bool) -> f32 {
    if !*playing {
        return 0.0;
    }
    match ring.pop_front() {
        Some(s) => s as f32 / 32768.0,
        None => {
            *playing = false;
            0.0
        }
    }
}

enum CaptureProcessor {
    Passthrough {
        sender: UnboundedSender<Vec<i16>>,
        frame: Vec<i16>,
        transmitting: Arc<AtomicBool>,
    },
    Resampling {
        resampler: Fft<f32>,
        /// 跨回调保留未处理完的输入样本
        in_ring: VecDeque<f32>,
        /// 已重采样、待打包成帧的输出样本
        frame: Vec<i16>,
        sender: UnboundedSender<Vec<i16>>,
        transmitting: Arc<AtomicBool>,
    },
}

impl CaptureProcessor {
    fn new(
        input_rate: u32,
        sender: UnboundedSender<Vec<i16>>,
        transmitting: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        if input_rate == TARGET_SAMPLE_RATE {
            eprintln!(
                "[Audio] Input: device supports {} Hz directly, using passthrough",
                input_rate
            );
            Ok(CaptureProcessor::Passthrough {
                sender,
                frame: Vec::with_capacity(VOICE_FRAME_SAMPLES),
                transmitting,
            })
        } else {
            eprintln!(
                "[Audio] Input: device {} Hz, creating resampler {} -> {} Hz",
                input_rate, input_rate, TARGET_SAMPLE_RATE
            );
            let resampler = Fft::<f32>::new(
                input_rate as usize,
                TARGET_SAMPLE_RATE as usize,
                VOICE_FRAME_SAMPLES,
                1,
                1,
                FixedSync::Both,
            )
            .map_err(|e| format!("failed to create capture resampler: {e}"))?;
            Ok(CaptureProcessor::Resampling {
                resampler,
                in_ring: VecDeque::with_capacity(VOICE_FRAME_SAMPLES * 4),
                frame: Vec::with_capacity(VOICE_FRAME_SAMPLES),
                sender,
                transmitting,
            })
        }
    }

    fn process(&mut self, mono_input: &[i16]) {
        match self {
            CaptureProcessor::Passthrough {
                sender,
                frame,
                transmitting,
            } => {
                if !transmitting.load(Ordering::Relaxed) {
                    frame.clear();
                    return;
                }
                for &sample in mono_input {
                    frame.push(sample);
                    if frame.len() == VOICE_FRAME_SAMPLES {
                        let out = std::mem::replace(frame, Vec::with_capacity(VOICE_FRAME_SAMPLES));
                        let _ = sender.send(out);
                    }
                }
            }
            CaptureProcessor::Resampling {
                resampler,
                in_ring,
                frame,
                sender,
                transmitting,
            } => {
                if !transmitting.load(Ordering::Relaxed) {
                    frame.clear();
                    in_ring.clear();
                    resampler.reset();
                    return;
                }

                // 把本次回调数据追加到跨回调 ring，再按 chunk_size 批量喂给 rubato
                for &s in mono_input {
                    in_ring.push_back(s as f32 / 32768.0);
                }

                let chunk_size = resampler.input_frames_next();
                while in_ring.len() >= chunk_size {
                    let chunk: Vec<f32> = in_ring.drain(..chunk_size).collect();
                    let input_data = vec![chunk];
                    let adapter = match SequentialSliceOfVecs::new(&input_data, 1, chunk_size) {
                        Ok(a) => a,
                        Err(e) => {
                            eprintln!("[Audio] capture adapter error: {e:?}");
                            break;
                        }
                    };
                    match resampler.process(&adapter, 0, None) {
                        Ok(interleaved) => {
                            if let Some(channel_iter) = interleaved.iter_channel(0) {
                                for sample in channel_iter {
                                    let s = (sample * 32768.0).clamp(-32768.0, 32767.0) as i16;
                                    frame.push(s);
                                    if frame.len() == VOICE_FRAME_SAMPLES {
                                        let out = std::mem::replace(
                                            frame,
                                            Vec::with_capacity(VOICE_FRAME_SAMPLES),
                                        );
                                        let _ = sender.send(out);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("[Audio] capture resample error: {e:?}");
                            break;
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct AudioEngine {
    inner: Arc<Mutex<AudioInner>>,
    transmitting: Arc<AtomicBool>,
    /// NRL 播放开关（NRL 静音按钮）；FMO 静音由接收侧 rx_play 门控
    monitoring_nrl: Arc<AtomicBool>,
    monitoring_fmo: Arc<AtomicBool>,
    capture_rx: Arc<Mutex<Option<UnboundedReceiver<Vec<i16>>>>>,
}

struct AudioInner {
    input_stream: Option<Stream>,
    /// NRL / FMO 各自一条输出流，混音交给操作系统
    output_stream_nrl: Option<Stream>,
    output_stream_fmo: Option<Stream>,
    playback_nrl: Arc<Mutex<PlaybackState>>,
    playback_fmo: Arc<Mutex<PlaybackState>>,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    aec_capture: Option<AecCapture>,
}

// cpal::Stream 在 macOS 上未实现 Send（CoreAudio 回调持有 dyn FnMut），
// 但 Stream 始终在 Mutex 保护下访问，实际访问路径是线程安全的。
unsafe impl Send for AudioInner {}

impl AudioEngine {
    pub fn new() -> Self {
        let transmitting = Arc::new(AtomicBool::new(false));
        let monitoring_nrl = Arc::new(AtomicBool::new(true));
        let monitoring_fmo = Arc::new(AtomicBool::new(true));
        let (capture_tx, capture_rx) = unbounded_channel();

        let dummy_nrl = Arc::new(Mutex::new(
            PlaybackState::new(TARGET_SAMPLE_RATE, monitoring_nrl.clone())
                .expect("8kHz passthrough playback state"),
        ));
        let dummy_fmo = Arc::new(Mutex::new(
            PlaybackState::new(TARGET_SAMPLE_RATE, monitoring_fmo.clone())
                .expect("8kHz passthrough playback state"),
        ));

        let _ = capture_tx;

        Self {
            inner: Arc::new(Mutex::new(AudioInner {
                input_stream: None,
                output_stream_nrl: None,
                output_stream_fmo: None,
                playback_nrl: dummy_nrl,
                playback_fmo: dummy_fmo,
                #[cfg(any(target_os = "windows", target_os = "macos"))]
                aec_capture: None,
            })),
            transmitting,
            monitoring_nrl,
            monitoring_fmo,
            capture_rx: Arc::new(Mutex::new(Some(capture_rx))),
        }
    }

    pub fn start(&self) -> Result<(DeviceSettings, Vec<String>), String> {
        // 幂等：已在运行则直接返回（双协议 NRL/FMO 各自 connect 时避免重建流）
        {
            let inner = self
                .inner
                .lock()
                .map_err(|_| "audio state poisoned".to_string())?;
            if inner.output_stream_nrl.is_some() {
                return Ok((
                    DeviceSettings {
                        input_device: "Running".into(),
                        output_device: "Running".into(),
                        sample_rate: TARGET_SAMPLE_RATE,
                        input_device_rate: TARGET_SAMPLE_RATE,
                        output_device_rate: TARGET_SAMPLE_RATE,
                        input_resampling: false,
                        output_resampling: false,
                        jitter_buffer_ms: 120,
                        agc_enabled: false,
                        noise_suppression: false,
                        aec_enabled: false,
                    },
                    vec!["音频引擎已在运行".into()],
                ));
            }
        }
        let host = cpal::default_host();
        let mut logs: Vec<String> = Vec::new();

        let output_device = host
            .default_output_device()
            .ok_or_else(|| "no default output device".to_string())?;
        let (output_supported, output_logs) = preferred_config(&output_device, false)
            .map_err(|err| format!("default output config failed: {err}"))?;
        logs.extend(output_logs);
        let output_name = output_device
            .name()
            .unwrap_or_else(|_| "Default Speaker".into());
        let output_rate = output_supported.sample_rate().0;
        let output_resampling = output_rate != TARGET_SAMPLE_RATE;

        // NRL / FMO 各自一条输出流（同一设备），混音交给操作系统
        let playback_nrl = Arc::new(Mutex::new(PlaybackState::new(
            output_rate,
            self.monitoring_nrl.clone(),
        )?));
        let playback_fmo = Arc::new(Mutex::new(PlaybackState::new(
            output_rate,
            self.monitoring_fmo.clone(),
        )?));

        let output_stream_nrl =
            build_output_stream(&output_device, &output_supported, playback_nrl.clone())?;
        output_stream_nrl
            .play()
            .map_err(|err| format!("start nrl output stream failed: {err}"))?;
        let output_stream_fmo =
            build_output_stream(&output_device, &output_supported, playback_fmo.clone())?;
        output_stream_fmo
            .play()
            .map_err(|err| format!("start fmo output stream failed: {err}"))?;

        let mut input_name = "Unavailable".to_string();
        let mut input_rate = TARGET_SAMPLE_RATE;
        let mut input_resampling = false;
        let mut input_stream = None;
        #[cfg_attr(
            not(any(target_os = "windows", target_os = "macos")),
            allow(unused_mut)
        )]
        let mut aec_enabled = false;
        {
            let mut rx_guard = self
                .capture_rx
                .lock()
                .map_err(|_| "capture receiver poisoned")?;
            *rx_guard = None;
        }

        // ── Windows / macOS: try platform AEC first ─────────────────────────
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        let mut aec_capture: Option<AecCapture> = None;

        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            let (capture_tx, capture_rx) = unbounded_channel();
            match AecCapture::start(capture_tx, self.transmitting.clone()) {
                Ok(aec) => {
                    input_name = format!("{} (AEC)", aec.device_name);
                    input_rate = aec.device_rate;
                    input_resampling = input_rate != TARGET_SAMPLE_RATE;
                    aec_enabled = true;
                    #[cfg(target_os = "windows")]
                    let backend = "Windows WASAPI";
                    #[cfg(target_os = "macos")]
                    let backend = "macOS VoiceProcessingIO";
                    logs.push(format!(
                        "AEC: {backend} echo cancellation active @ {} Hz",
                        input_rate
                    ));
                    eprintln!("[AEC] {backend} AEC active: {}", input_name);
                    {
                        let mut rx_guard = self
                            .capture_rx
                            .lock()
                            .map_err(|_| "capture receiver poisoned")?;
                        *rx_guard = Some(capture_rx);
                    }
                    aec_capture = Some(aec);
                }
                Err(e) => {
                    eprintln!("[AEC] WASAPI AEC unavailable ({e}), falling back to cpal");
                    logs.push(format!("AEC unavailable: {e}"));
                }
            }
        }

        // ── Fallback: cpal default input ────────────────────────────────────
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let use_cpal_input = true;
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        let use_cpal_input = aec_capture.is_none();

        if use_cpal_input {
            if let Some(device) = host.default_input_device() {
                input_name = device
                    .name()
                    .unwrap_or_else(|_| "Default Microphone".into());
                if let Ok((input_supported, input_logs)) = preferred_config(&device, true) {
                    logs.extend(input_logs);
                    input_rate = input_supported.sample_rate().0;
                    input_resampling = input_rate != TARGET_SAMPLE_RATE;
                    let (capture_tx, capture_rx) = unbounded_channel();
                    if let Ok(stream) = build_input_stream(
                        &device,
                        &input_supported,
                        capture_tx,
                        self.transmitting.clone(),
                    ) {
                        if stream.play().is_ok() {
                            {
                                let mut rx_guard = self
                                    .capture_rx
                                    .lock()
                                    .map_err(|_| "capture receiver poisoned")?;
                                *rx_guard = Some(capture_rx);
                            }
                            input_stream = Some(stream);
                        } else {
                            input_name = format!("{input_name} (start failed)");
                        }
                    } else {
                        input_name = format!("{input_name} (unsupported)");
                    }
                } else {
                    input_name = format!("{input_name} (config failed)");
                }
            }
        }

        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "audio state poisoned".to_string())?;
        inner.playback_nrl = playback_nrl;
        inner.playback_fmo = playback_fmo;
        inner.output_stream_nrl = Some(output_stream_nrl);
        inner.output_stream_fmo = Some(output_stream_fmo);
        inner.input_stream = input_stream;
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            inner.aec_capture = aec_capture;
        }

        Ok((
            DeviceSettings {
                input_device: input_name,
                output_device: output_name,
                sample_rate: TARGET_SAMPLE_RATE,
                input_device_rate: input_rate,
                output_device_rate: output_rate,
                input_resampling,
                output_resampling,
                jitter_buffer_ms: 120,
                agc_enabled: false,
                noise_suppression: false,
                aec_enabled,
            },
            logs,
        ))
    }

    pub fn stop(&self) {
        self.transmitting.store(false, Ordering::Relaxed);
        if let Ok(mut inner) = self.inner.lock() {
            inner.input_stream = None;
            inner.output_stream_nrl = None;
            inner.output_stream_fmo = None;
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            {
                inner.aec_capture = None;
            }
            for playback in [&inner.playback_nrl, &inner.playback_fmo] {
                if let Ok(mut pb) = playback.lock() {
                    pb.ring.clear();
                    pb.out_buf.clear();
                    pb.playing = false;
                    if let Some(r) = pb.resampler.as_mut() {
                        r.reset();
                    }
                }
            }
        }
    }

    pub fn set_transmitting(&self, enabled: bool) {
        self.transmitting.store(enabled, Ordering::Relaxed);
    }

    /// NRL 播放开关（NRL 静音按钮）：只影响 NRL 输出流，FMO 不受影响
    pub fn set_monitoring(&self, enabled: bool) {
        self.monitoring_nrl.store(enabled, Ordering::Relaxed);
    }

    pub fn enqueue_received_pcm(&self, pcm: &[i16], channel: RxChannel) {
        if let Ok(inner) = self.inner.lock() {
            let playback = match channel {
                RxChannel::Nrl => &inner.playback_nrl,
                RxChannel::Fmo => &inner.playback_fmo,
            };
            if let Ok(mut pb) = playback.lock() {
                pb.enqueue(pcm);
            }
        }
    }

    pub fn take_capture_receiver(&self) -> Option<UnboundedReceiver<Vec<i16>>> {
        self.capture_rx.lock().ok()?.take()
    }
}

fn build_input_stream(
    device: &cpal::Device,
    supported: &SupportedStreamConfig,
    sender: UnboundedSender<Vec<i16>>,
    transmitting: Arc<AtomicBool>,
) -> Result<Stream, String> {
    let config: StreamConfig = supported.clone().into();
    let channels = config.channels as usize;
    eprintln!(
        "[Audio] Input stream: {}Hz, {} ch, format={:?}",
        config.sample_rate.0,
        channels,
        supported.sample_format()
    );
    let state = Arc::new(Mutex::new(CaptureProcessor::new(
        config.sample_rate.0,
        sender,
        transmitting,
    )?));
    match supported.sample_format() {
        SampleFormat::F32 => {
            let state = state.clone();
            device
                .build_input_stream(
                    &config,
                    move |data: &[f32], _| capture_callback_f32(data, channels, &state),
                    |err| eprintln!("input stream error: {err}"),
                    None,
                )
                .map_err(|err| format!("build input stream failed: {err}"))
        }
        SampleFormat::I16 => {
            let state = state.clone();
            device
                .build_input_stream(
                    &config,
                    move |data: &[i16], _| capture_callback_i16(data, channels, &state),
                    |err| eprintln!("input stream error: {err}"),
                    None,
                )
                .map_err(|err| format!("build input stream failed: {err}"))
        }
        SampleFormat::U16 => {
            let state = state.clone();
            device
                .build_input_stream(
                    &config,
                    move |data: &[u16], _| capture_callback_u16(data, channels, &state),
                    |err| eprintln!("input stream error: {err}"),
                    None,
                )
                .map_err(|err| format!("build input stream failed: {err}"))
        }
        sample_format => Err(format!("unsupported input format: {sample_format:?}")),
    }
}

fn build_output_stream(
    device: &cpal::Device,
    supported: &SupportedStreamConfig,
    playback: Arc<Mutex<PlaybackState>>,
) -> Result<Stream, String> {
    let config: StreamConfig = supported.clone().into();
    let channels = config.channels as usize;
    eprintln!(
        "[Audio] Output stream: {}Hz, {} ch, format={:?}",
        config.sample_rate.0,
        channels,
        supported.sample_format()
    );
    match supported.sample_format() {
        SampleFormat::F32 => {
            let playback = playback.clone();
            device
                .build_output_stream(
                    &config,
                    move |data: &mut [f32], _| render_output_f32(data, channels, &playback),
                    |err| eprintln!("output stream error: {err}"),
                    None,
                )
                .map_err(|err| format!("build output stream failed: {err}"))
        }
        SampleFormat::I16 => {
            let playback = playback.clone();
            device
                .build_output_stream(
                    &config,
                    move |data: &mut [i16], _| render_output_i16(data, channels, &playback),
                    |err| eprintln!("output stream error: {err}"),
                    None,
                )
                .map_err(|err| format!("build output stream failed: {err}"))
        }
        SampleFormat::U16 => {
            let playback = playback.clone();
            device
                .build_output_stream(
                    &config,
                    move |data: &mut [u16], _| render_output_u16(data, channels, &playback),
                    |err| eprintln!("output stream error: {err}"),
                    None,
                )
                .map_err(|err| format!("build output stream failed: {err}"))
        }
        sample_format => Err(format!("unsupported output format: {sample_format:?}")),
    }
}

fn capture_callback_f32(data: &[f32], channels: usize, state: &Arc<Mutex<CaptureProcessor>>) {
    let mono = interleaved_to_mono_i16_f32(data, channels);
    if let Ok(mut guard) = state.lock() {
        guard.process(&mono);
    }
}

fn capture_callback_i16(data: &[i16], channels: usize, state: &Arc<Mutex<CaptureProcessor>>) {
    let mono = interleaved_to_mono_i16_i16(data, channels);
    if let Ok(mut guard) = state.lock() {
        guard.process(&mono);
    }
}

fn capture_callback_u16(data: &[u16], channels: usize, state: &Arc<Mutex<CaptureProcessor>>) {
    let mono = interleaved_to_mono_i16_u16(data, channels);
    if let Ok(mut guard) = state.lock() {
        guard.process(&mono);
    }
}

fn render_output_f32(data: &mut [f32], channels: usize, playback: &Arc<Mutex<PlaybackState>>) {
    render_output(data, channels, playback, |slot, sample| {
        for out in slot.iter_mut() {
            *out = sample;
        }
    });
}

fn render_output_i16(data: &mut [i16], channels: usize, playback: &Arc<Mutex<PlaybackState>>) {
    render_output(data, channels, playback, |slot, sample| {
        let value = (sample * 32767.0).clamp(-32768.0, 32767.0) as i16;
        slot.fill(value);
    });
}

fn render_output_u16(data: &mut [u16], channels: usize, playback: &Arc<Mutex<PlaybackState>>) {
    render_output(data, channels, playback, |slot, sample| {
        let value = (sample * 32767.0 + 32768.0).clamp(0.0, 65535.0) as u16;
        slot.fill(value);
    });
}

fn render_output<T, F>(
    data: &mut [T],
    channels: usize,
    playback: &Arc<Mutex<PlaybackState>>,
    mut writer: F,
) where
    F: FnMut(&mut [T], f32),
{
    if channels == 0 {
        return;
    }

    let frame_count = data.len() / channels;

    if let Ok(mut state) = playback.lock() {
        let batch = state.next_batch(frame_count);
        for (frame_index, chunk) in data.chunks_mut(channels).enumerate() {
            let sample = batch.get(frame_index).copied().unwrap_or(0.0);
            writer(chunk, sample);
        }
    } else {
        for chunk in data.chunks_mut(channels) {
            writer(chunk, 0.0);
        }
    }
}

fn interleaved_to_mono_i16_f32(data: &[f32], channels: usize) -> Vec<i16> {
    data.chunks(channels.max(1))
        .map(|frame| {
            let sum: f32 = frame.iter().copied().sum();
            let avg = sum / frame.len().max(1) as f32;
            (avg.clamp(-1.0, 1.0) * 32767.0) as i16
        })
        .collect()
}

fn interleaved_to_mono_i16_i16(data: &[i16], channels: usize) -> Vec<i16> {
    data.chunks(channels.max(1))
        .map(|frame| {
            let sum: i32 = frame.iter().map(|sample| *sample as i32).sum();
            (sum / frame.len().max(1) as i32) as i16
        })
        .collect()
}

fn interleaved_to_mono_i16_u16(data: &[u16], channels: usize) -> Vec<i16> {
    data.chunks(channels.max(1))
        .map(|frame| {
            let sum: i32 = frame.iter().map(|sample| *sample as i32 - 32768).sum();
            (sum / frame.len().max(1) as i32) as i16
        })
        .collect()
}

/// 尝试返回设备原生支持 8000 Hz 的配置，否则回退到设备默认配置。
/// 同时返回供前端展示的日志行。
fn preferred_config(
    device: &cpal::Device,
    is_input: bool,
) -> Result<(SupportedStreamConfig, Vec<String>), cpal::DefaultStreamConfigError> {
    let label = if is_input { "输入" } else { "输出" };
    let target = cpal::SampleRate(TARGET_SAMPLE_RATE);
    let mut logs: Vec<String> = Vec::new();

    let ranges: Result<Box<dyn Iterator<Item = cpal::SupportedStreamConfigRange>>, _> = if is_input
    {
        device
            .supported_input_configs()
            .map(|it| -> Box<dyn Iterator<Item = _>> { Box::new(it) })
            .map_err(|_| ())
    } else {
        device
            .supported_output_configs()
            .map(|it| -> Box<dyn Iterator<Item = _>> { Box::new(it) })
            .map_err(|_| ())
    };

    if let Ok(ranges) = ranges {
        // 优先选 mono+8000Hz，其次多声道+8000Hz
        let mut best_multi: Option<SupportedStreamConfig> = None;
        for range in ranges {
            let line = format!(
                "{label}设备支持: {}声道 {}-{}Hz {:?}",
                range.channels(),
                range.min_sample_rate().0,
                range.max_sample_rate().0,
                range.sample_format(),
            );
            eprintln!("[Audio] {line}");
            logs.push(line);

            if range.min_sample_rate() <= target && target <= range.max_sample_rate() {
                let config = range.with_sample_rate(target);
                if config.channels() == 1 {
                    let line = format!(
                        "{label}设备选用: mono 8000Hz {:?}，无需重采样",
                        config.sample_format()
                    );
                    eprintln!("[Audio] {line}");
                    logs.push(line);
                    return Ok((config, logs));
                } else if best_multi.is_none() {
                    best_multi = Some(config);
                }
            }
        }
        if let Some(config) = best_multi {
            let line = format!(
                "{label}设备选用: {}声道 8000Hz {:?}，无需重采样（无mono）",
                config.channels(),
                config.sample_format()
            );
            eprintln!("[Audio] {line}");
            logs.push(line);
            return Ok((config, logs));
        }
    }

    // 设备不支持 8000 Hz，使用默认配置，后续走重采样路径
    let fallback = if is_input {
        device.default_input_config()
    } else {
        device.default_output_config()
    };
    if let Ok(ref c) = fallback {
        let line = format!(
            "{label}设备不支持8000Hz，使用默认: {}声道 {}Hz {:?}，将重采样",
            c.channels(),
            c.sample_rate().0,
            c.sample_format(),
        );
        eprintln!("[Audio] {line}");
        logs.push(line);
    }
    fallback.map(|c| (c, logs))
}
