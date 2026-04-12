use audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::audioadapter::AdapterIterators;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig, SupportedStreamConfig};
use rubato::{Fft, FixedSync, Resampler};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::models::DeviceSettings;
#[cfg(target_os = "windows")]
use crate::audio_aec_win::AecCapture;
#[cfg(target_os = "macos")]
use crate::audio_aec_mac::AecCapture;

const TARGET_SAMPLE_RATE: u32 = 8_000;
const VOICE_FRAME_SAMPLES: usize = 160;
// 回放 ring buffer 最多缓存 500ms 的输入样本，超出丢弃以防积压延迟
const MAX_RING_SAMPLES: usize = TARGET_SAMPLE_RATE as usize / 2;

enum PlaybackRenderer {
    Passthrough(VecDeque<i16>),
    Resampling {
        /// 待重采样的输入样本（8kHz f32）
        ring: VecDeque<f32>,
        /// 已重采样、待输出的样本（设备采样率 f32）
        out_buf: VecDeque<f32>,
        resampler: Fft<f32>,
    },
}

struct PlaybackState {
    renderer: PlaybackRenderer,
    monitoring: Arc<AtomicBool>,
}

impl PlaybackState {
    fn new(output_rate: u32, monitoring: Arc<AtomicBool>) -> Result<Self, String> {
        let renderer = if output_rate == TARGET_SAMPLE_RATE {
            eprintln!(
                "[Audio] Output: device supports {} Hz directly, using passthrough",
                output_rate
            );
            PlaybackRenderer::Passthrough(VecDeque::with_capacity(TARGET_SAMPLE_RATE as usize))
        } else {
            eprintln!(
                "[Audio] Output: device {} Hz, creating resampler {} -> {} Hz",
                output_rate, TARGET_SAMPLE_RATE, output_rate
            );
            let resampler = Fft::<f32>::new(
                TARGET_SAMPLE_RATE as usize,
                output_rate as usize,
                VOICE_FRAME_SAMPLES,
                1,
                1,
                FixedSync::Both,
            )
            .map_err(|e| format!("failed to create resampler: {e}"))?;
            PlaybackRenderer::Resampling {
                ring: VecDeque::with_capacity(MAX_RING_SAMPLES * 2),
                out_buf: VecDeque::with_capacity(MAX_RING_SAMPLES * 2),
                resampler,
            }
        };

        Ok(Self {
            renderer,
            monitoring,
        })
    }

    fn enqueue(&mut self, pcm: &[i16]) {
        match &mut self.renderer {
            PlaybackRenderer::Passthrough(buf) => {
                buf.extend(pcm.iter().copied());
                // 防止积压：超过 500ms 的数据直接丢弃头部
                while buf.len() > MAX_RING_SAMPLES {
                    buf.pop_front();
                }
            }
            PlaybackRenderer::Resampling { ring, out_buf, resampler } => {
                for &sample in pcm.iter() {
                    ring.push_back(sample as f32 / 32768.0);
                }
                // 防止积压：超过 500ms 的输入样本直接丢弃头部
                while ring.len() > MAX_RING_SAMPLES {
                    ring.pop_front();
                }
                // 把 ring 里够 chunk_size 的数据立即重采样，结果存入 out_buf
                let chunk_size = resampler.input_frames_next();
                while ring.len() >= chunk_size {
                    let chunk: Vec<f32> = ring.drain(..chunk_size).collect();
                    let input_data = vec![chunk];
                    if let Ok(adapter) = SequentialSliceOfVecs::new(&input_data, 1, chunk_size) {
                        if let Ok(interleaved) = resampler.process(&adapter, 0, None) {
                            if let Some(iter) = interleaved.iter_channel(0) {
                                for s in iter {
                                    out_buf.push_back(s);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn next_batch(&mut self, output_frames: usize) -> Vec<f32> {
        if !self.monitoring.load(Ordering::Relaxed) {
            return vec![0.0; output_frames];
        }

        match &mut self.renderer {
            PlaybackRenderer::Passthrough(buf) => {
                let mut output = Vec::with_capacity(output_frames);
                for _ in 0..output_frames {
                    output.push(match buf.pop_front() {
                        Some(s) => s as f32 / 32768.0,
                        None => 0.0,
                    });
                }
                output
            }
            PlaybackRenderer::Resampling { out_buf, .. } => {
                let mut output = Vec::with_capacity(output_frames);
                for _ in 0..output_frames {
                    output.push(out_buf.pop_front().unwrap_or(0.0));
                }
                output
            }
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
            CaptureProcessor::Passthrough { sender, frame, transmitting } => {
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
                                        let out = std::mem::replace(frame, Vec::with_capacity(VOICE_FRAME_SAMPLES));
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
    monitoring: Arc<AtomicBool>,
    capture_rx: Arc<Mutex<Option<UnboundedReceiver<Vec<i16>>>>>,
}

struct AudioInner {
    input_stream: Option<Stream>,
    output_stream: Option<Stream>,
    playback: Arc<Mutex<PlaybackState>>,
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    aec_capture: Option<AecCapture>,
}

// cpal::Stream 在 macOS 上未实现 Send（CoreAudio 回调持有 dyn FnMut），
// 但 Stream 始终在 Mutex 保护下访问，实际访问路径是线程安全的。
unsafe impl Send for AudioInner {}

impl AudioEngine {
    pub fn new() -> Self {
        let transmitting = Arc::new(AtomicBool::new(false));
        let monitoring = Arc::new(AtomicBool::new(true));
        let (capture_tx, capture_rx) = unbounded_channel();

        let dummy_playback = Arc::new(Mutex::new(
            PlaybackState::new(TARGET_SAMPLE_RATE, monitoring.clone()).unwrap_or_else(|_| {
                PlaybackState {
                    renderer: PlaybackRenderer::Passthrough(VecDeque::new()),
                    monitoring: monitoring.clone(),
                }
            }),
        ));

        let _ = capture_tx;

        Self {
            inner: Arc::new(Mutex::new(AudioInner {
                input_stream: None,
                output_stream: None,
                playback: dummy_playback,
                #[cfg(any(target_os = "windows", target_os = "macos"))]
                aec_capture: None,
            })),
            transmitting,
            monitoring,
            capture_rx: Arc::new(Mutex::new(Some(capture_rx))),
        }
    }

    pub fn start(&self) -> Result<(DeviceSettings, Vec<String>), String> {
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

        let playback = Arc::new(Mutex::new(PlaybackState::new(
            output_rate,
            self.monitoring.clone(),
        )?));

        let output_stream =
            build_output_stream(&output_device, &output_supported, playback.clone())?;
        output_stream
            .play()
            .map_err(|err| format!("start output stream failed: {err}"))?;

        let mut input_name = "Unavailable".to_string();
        let mut input_rate = TARGET_SAMPLE_RATE;
        let mut input_resampling = false;
        let mut input_stream = None;
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
                    logs.push(format!(
                        "AEC: Windows WASAPI echo cancellation active @ {} Hz",
                        input_rate
                    ));
                    eprintln!("[AEC] WASAPI AEC active: {}", input_name);
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
        inner.playback = playback;
        inner.output_stream = Some(output_stream);
        inner.input_stream = input_stream;
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        { inner.aec_capture = aec_capture; }

        Ok((DeviceSettings {
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
        }, logs))
    }

    pub fn stop(&self) {
        self.transmitting.store(false, Ordering::Relaxed);
        if let Ok(mut inner) = self.inner.lock() {
            inner.input_stream = None;
            inner.output_stream = None;
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            { inner.aec_capture = None; }
            if let Ok(mut playback) = inner.playback.lock() {
                match &mut playback.renderer {
                    PlaybackRenderer::Passthrough(buf) => buf.clear(),
                    PlaybackRenderer::Resampling { ring, out_buf, resampler } => {
                        ring.clear();
                        out_buf.clear();
                        resampler.reset();
                    }
                }
            }
        }
    }

    pub fn set_transmitting(&self, enabled: bool) {
        self.transmitting.store(enabled, Ordering::Relaxed);
    }

    pub fn set_monitoring(&self, enabled: bool) {
        self.monitoring.store(enabled, Ordering::Relaxed);
    }

    pub fn enqueue_received_pcm(&self, pcm: &[i16]) {
        if let Ok(inner) = self.inner.lock() {
            if let Ok(mut playback) = inner.playback.lock() {
                playback.enqueue(pcm);
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
        config.sample_rate.0, channels, supported.sample_format()
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
        config.sample_rate.0, channels, supported.sample_format()
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
