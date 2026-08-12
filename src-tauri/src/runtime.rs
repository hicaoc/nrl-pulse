use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::RwLock;

use crate::audio::{AudioEngine, RxChannel};
use crate::config::{RuntimeConfig, SerialTunnelConfig};
use crate::fmo::state::FmoState;
use crate::models::{
    AtState, ChatMessageEvent, DeviceSettings, PresenceItem, RealtimeAudioState, RuntimeBootstrap,
    SerialTunnelSnapshot, SessionSnapshot, TimelineEvent,
};
use crate::opus::{NrlOpusDecoder, NrlOpusEncoder};
use crate::serial_tunnel::SerialTunnel;
use crate::udp::UdpSession;

fn chrono_local_now() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

fn voice_file_path(
    callsign: &str,
    ssid: u8,
    duration_ms: u128,
    save_path: &str,
) -> Result<PathBuf, String> {
    let base_dir = if save_path.is_empty() {
        dirs::audio_dir().unwrap_or_else(|| PathBuf::from("."))
    } else {
        PathBuf::from(save_path)
    };

    let date_dir = chrono::Local::now().format("%Y-%m-%d").to_string();
    let full_dir = base_dir.join(&date_dir);
    std::fs::create_dir_all(&full_dir).map_err(|e| format!("Failed to create directory: {}", e))?;

    let timestamp = chrono::Local::now().format("%H%M%S");
    let duration_sec = (duration_ms as f64 / 1000.0).max(1.0);
    let duration_str = format!("{:.1}", duration_sec);
    let filename = format!("{}-{}_{}_{}s.wav", callsign, ssid, timestamp, duration_str);
    Ok(full_dir.join(&filename))
}

fn save_voice_to_wav(file_path: &Path, audio_data: &[i16]) -> Result<(), String> {
    if audio_data.is_empty() {
        return Err("No audio data to save".into());
    }

    let sample_rate: u32 = 8000;
    let num_channels: u16 = 1;
    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * u32::from(num_channels) * u32::from(bits_per_sample / 8);
    let block_align = num_channels * (bits_per_sample / 8);
    let data_size = audio_data.len() * 2;

    let mut file = File::create(file_path).map_err(|e| format!("Failed to create file: {}", e))?;

    file.write_all(b"RIFF")
        .map_err(|e| format!("Failed to write RIFF: {}", e))?;

    let chunk_size: u32 = 36 + data_size as u32;
    file.write_all(&chunk_size.to_le_bytes())
        .map_err(|e| format!("Failed to write chunk size: {}", e))?;

    file.write_all(b"WAVE")
        .map_err(|e| format!("Failed to write WAVE: {}", e))?;

    file.write_all(b"fmt ")
        .map_err(|e| format!("Failed to write fmt: {}", e))?;

    let subchunk1_size: u32 = 16;
    file.write_all(&subchunk1_size.to_le_bytes())
        .map_err(|e| format!("Failed to write subchunk1 size: {}", e))?;

    let audio_format: u16 = 1;
    file.write_all(&audio_format.to_le_bytes())
        .map_err(|e| format!("Failed to write audio format: {}", e))?;

    file.write_all(&num_channels.to_le_bytes())
        .map_err(|e| format!("Failed to write num channels: {}", e))?;

    file.write_all(&sample_rate.to_le_bytes())
        .map_err(|e| format!("Failed to write sample rate: {}", e))?;

    file.write_all(&byte_rate.to_le_bytes())
        .map_err(|e| format!("Failed to write byte rate: {}", e))?;

    file.write_all(&block_align.to_le_bytes())
        .map_err(|e| format!("Failed to write block align: {}", e))?;

    file.write_all(&bits_per_sample.to_le_bytes())
        .map_err(|e| format!("Failed to write bits per sample: {}", e))?;

    file.write_all(b"data")
        .map_err(|e| format!("Failed to write data: {}", e))?;

    file.write_all(&(data_size as u32).to_le_bytes())
        .map_err(|e| format!("Failed to write data size: {}", e))?;

    for &sample in audio_data {
        file.write_all(&sample.to_le_bytes())
            .map_err(|e| format!("Failed to write sample: {}", e))?;
    }

    Ok(())
}

fn build_waveform_preview(audio_data: &[i16], samples: usize) -> Vec<f32> {
    if audio_data.is_empty() || samples == 0 {
        return Vec::new();
    }

    let step = (audio_data.len() / samples).max(1);
    let mut waveform = Vec::with_capacity(samples);
    for i in 0..samples {
        let idx = i * step;
        let mut sum = 0.0f32;
        let mut count = 0usize;
        for j in 0..step {
            let Some(sample) = audio_data.get(idx + j) else {
                break;
            };
            sum += (*sample as f32 / i16::MAX as f32).abs();
            count += 1;
        }
        waveform.push(if count > 0 { sum / count as f32 } else { 0.0 });
    }
    waveform
}

#[derive(Clone)]
pub struct RuntimeState {
    inner: Arc<RwLock<RuntimeData>>,
    udp: UdpSession,
    serial_tunnel: SerialTunnel,
    audio: AudioEngine,
    app: Arc<RwLock<Option<AppHandle>>>,
    /// FMO 运行时状态（NRL 与 FMO 协议共存）
    fmo: Arc<RwLock<Option<Arc<FmoState>>>>,
    /// NRL type=8 Opus 发射编码器
    nrl_opus_tx: Arc<std::sync::Mutex<Option<NrlOpusEncoder>>>,
    /// NRL type=8 Opus 接收解码器
    nrl_opus_rx: Arc<std::sync::Mutex<Option<NrlOpusDecoder>>>,
    capture_task_running: Arc<AtomicBool>,
    voice_watchdog_running: Arc<AtomicBool>,
    heartbeat_watchdog_running: Arc<AtomicBool>,
    /// 上次向前端 emit snapshot 的时间（ms），用于限速，避免每包都 emit 导致事件积压
    last_snapshot_emit_ms: Arc<AtomicU64>,
    /// 上次向前端 emit audio-state 的时间（ms），用于高频音频状态单独限速
    last_audio_emit_ms: Arc<AtomicU64>,
    /// 当前接收语音编码（"G711" | "OPUS"）
    rx_codec: Arc<std::sync::Mutex<String>>,
    /// 接收语音帧序号（每帧递增）
    rx_voice_seq: Arc<AtomicU64>,
    /// 最近一次收到 NRL UDP 报文的时间（epoch ms，0=从未收到）
    nrl_last_rx_ms: Arc<AtomicU64>,
}

struct RuntimeData {
    config: RuntimeConfig,
    snapshot: SessionSnapshot,
    presence: Vec<PresenceItem>,
    timeline: Vec<TimelineEvent>,
    at_state: AtState,
    /// 当前发射协议（"nrl" | "fmo"），独立于 config.protocol，供双协议各自 PTT
    tx_protocol: String,
    /// NRL 链路是否已连接（独立于 FMO，供 NRL box 状态显示）
    nrl_connected: bool,
    /// 接收录音会话按协议分槽：NRL/FMO 同时来语音时互不顶会话，
    /// 避免交替产生大量 0 秒语音消息
    voice_session_nrl: Option<VoiceSession>,
    voice_session_fmo: Option<VoiceSession>,
    outgoing_voice_data: Vec<i16>,
    outgoing_voice_start: Option<Instant>,
    last_heartbeat_at: Option<Instant>,
    heartbeat_timeout_reported: bool,
    last_heartbeat_sent_at: Option<Instant>,
    latency_ewma_ms: Option<f32>,
    last_voice_arrival_at: Option<Instant>,
    jitter_ewma_ms: Option<f32>,
    tick: u64,
}

#[derive(Clone)]
struct VoiceSession {
    callsign: String,
    ssid: u8,
    /// 来源协议（"nrl" | "fmo"），用于录音消息标题前缀
    protocol: &'static str,
    started_at: Instant,
    last_seen_at: Instant,
    audio_data: Vec<i16>,
}

const SPECTRUM_BANDS: usize = 28;
const MAX_VOICE_SESSION_SAMPLES: usize = 60 * 8000;
const SPECTRUM_FFT_SIZE: usize = 256;
const SPECTRUM_SAMPLE_RATE: f32 = 8_000.0;
const SPECTRUM_MIN_FREQ: f32 = 80.0;
const SPECTRUM_MAX_FREQ: f32 = 3_400.0;

pub(crate) struct FrameAnalysis {
    pub level: f32,
    pub spectrum: [f32; SPECTRUM_BANDS],
}

fn spectrum_fft() -> &'static Arc<dyn Fft<f32>> {
    static FFT: OnceLock<Arc<dyn Fft<f32>>> = OnceLock::new();
    FFT.get_or_init(|| {
        let mut planner = FftPlanner::<f32>::new();
        planner.plan_fft_forward(SPECTRUM_FFT_SIZE)
    })
}

impl RuntimeState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(RuntimeData::seed())),
            udp: UdpSession::new(),
            serial_tunnel: SerialTunnel::new(),
            audio: AudioEngine::new(),
            app: Arc::new(RwLock::new(None)),
            fmo: Arc::new(RwLock::new(None)),
            nrl_opus_tx: Arc::new(std::sync::Mutex::new(None)),
            nrl_opus_rx: Arc::new(std::sync::Mutex::new(None)),
            capture_task_running: Arc::new(AtomicBool::new(false)),
            voice_watchdog_running: Arc::new(AtomicBool::new(false)),
            heartbeat_watchdog_running: Arc::new(AtomicBool::new(false)),
            last_snapshot_emit_ms: Arc::new(AtomicU64::new(0)),
            last_audio_emit_ms: Arc::new(AtomicU64::new(0)),
            rx_codec: Arc::new(std::sync::Mutex::new(String::new())),
            rx_voice_seq: Arc::new(AtomicU64::new(0)),
            nrl_last_rx_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 记录收到任意 NRL UDP 报文（语音/心跳等），供链路活跃判定（10s 无报文则闪烁）
    pub fn note_nrl_rx(&self) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.nrl_last_rx_ms.store(now_ms, Ordering::Relaxed);
    }

    /// 记录一帧接收语音的编码类型（NRL：G711/OPUS），并递增帧序号
    pub fn note_rx_voice_frame_codec(&self, codec: &str) {
        if let Ok(mut c) = self.rx_codec.lock() {
            *c = codec.to_string();
        }
        self.rx_voice_seq.fetch_add(1, Ordering::Relaxed);
    }

    pub async fn set_app_handle(&self, app: AppHandle) {
        let mut guard = self.app.write().await;
        *guard = Some(app);
    }

    /// 挂载 FMO 运行时状态：设置 app、安装 raw 解码、桥接事件到 runtime。
    pub async fn attach_fmo(&self, fmo: Arc<FmoState>) {
        let app = self.app.read().await.clone();
        if let Some(app) = app {
            fmo.set_app(app).await;
        }
        // 事件桥接：FMO log/mqtt/aprs 事件 → runtime timeline/snapshot
        let runtime = self.clone();
        let bridge: Arc<dyn Fn(serde_json::Value) + Send + Sync> = Arc::new(move |ev| {
            let ty = ev.get("type").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let runtime = runtime.clone();
            tauri::async_runtime::spawn(async move {
                match ty.as_str() {
                    "log" => {
                        let level = ev.get("level").and_then(|l| l.as_str()).unwrap_or("info");
                        let msg = ev.get("msg").and_then(|m| m.as_str()).unwrap_or("").to_string();
                        let tone = match level {
                            "error" => "warn",
                            "warn" => "warn",
                            _ => "info",
                        };
                        runtime.push_runtime_event("FMO 事件", &msg, tone).await;
                    }
                    "mqtt_state" => {
                        let state = ev.get("state").and_then(|s| s.as_str()).unwrap_or("").to_string();
                        let detail = ev.get("detail").and_then(|d| d.as_str()).unwrap_or("").to_string();
                        runtime.apply_fmo_mqtt_state(&state, &detail).await;
                    }
                    "aprs_state" => {
                        let detail = ev.get("detail").and_then(|d| d.as_str()).unwrap_or("").to_string();
                        runtime.push_runtime_event("FMO APRS", &detail, "info").await;
                    }
                    _ => {}
                }
            });
        });
        *fmo.bridge.lock().unwrap() = Some(bridge);

        // FMO 解码 PCM → 本地回放 + FMO 独立指标 + 独立频谱推送
        let runtime = self.clone();
        let speaker = fmo.current_speaker.clone();
        let fmo_stats = fmo.stats.clone();
        let fmo_last_arrival = std::sync::Arc::new(std::sync::Mutex::new(None::<Instant>));
        *fmo.rx_audio.on_pcm.lock().unwrap() = Some(Box::new(move |pcm: &[i16]| {
            let samples = pcm.to_vec();
            let callsign = speaker.lock().unwrap().clone();
            let runtime = runtime.clone();
            let fmo_stats = fmo_stats.clone();
            let fmo_last_arrival = fmo_last_arrival.clone();
            // 同步入队：保证 40ms 音频块严格按到达顺序进播放缓冲。
            // 之前放在下方 spawn 的异步任务里，多线程 runtime 并行调度会乱序，听感为破音。
            runtime.enqueue_received_pcm(&samples, RxChannel::Fmo);
            tauri::async_runtime::spawn(async move {
                let analysis = analyze_pcm_frame(&samples);
                let sample_count = samples.len();
                // FMO 独立显示电平/频谱 + 抖动/队列/码率（不覆盖 NRL）
                {
                    let now = Instant::now();
                    let expected_ms = (sample_count as f32 / 8.0).max(1.0);
                    let mut jitter = *fmo_stats.jitter_ms.lock().unwrap();
                    if let Some(prev) = *fmo_last_arrival.lock().unwrap() {
                        let delta_ms = now.saturating_duration_since(prev).as_secs_f32() * 1000.0;
                        let dev = (delta_ms - expected_ms).abs();
                        let next = jitter as f32 + (dev - jitter as f32) / 16.0;
                        jitter = next.round().max(0.0) as u32;
                    }
                    *fmo_last_arrival.lock().unwrap() = Some(now);
                    *fmo_stats.jitter_ms.lock().unwrap() = jitter;
                    *fmo_stats.queued_frames.lock().unwrap() = (sample_count / 160).max(1) as u32;
                    *fmo_stats.downlink_kbps.lock().unwrap() =
                        ((sample_count / 2 * 8) as f32) / 1000.0;
                    *fmo_stats.rx_level.lock().unwrap() = analysis.level;
                    *fmo_stats.rx_spectrum.lock().unwrap() = analysis.spectrum.to_vec();
                }
                runtime
                    .note_fmo_voice_frame(
                        callsign,
                        sample_count,
                        analysis.level,
                        &analysis.spectrum,
                        &samples,
                    )
                    .await;
                // 高频独立事件：驱动 FMO 顶栏 VU / 频谱实时刷新（与 NRL 分开）
                if let Some(app) = runtime.app.read().await.clone() {
                    let _ = app.emit(
                        "fmo://audio-state",
                        serde_json::json!({
                            "rxLevel": *fmo_stats.rx_level.lock().unwrap(),
                            "rxSpectrum": fmo_stats.rx_spectrum.lock().unwrap().clone(),
                            "txLevel": *fmo_stats.tx_level.lock().unwrap(),
                            "txSpectrum": fmo_stats.tx_spectrum.lock().unwrap().clone(),
                            "jitterMs": *fmo_stats.jitter_ms.lock().unwrap(),
                            "queuedFrames": *fmo_stats.queued_frames.lock().unwrap(),
                            "downlinkKbps": *fmo_stats.downlink_kbps.lock().unwrap(),
                            "rxCodec": fmo_stats.rx_codec.lock().unwrap().clone(),
                            "rxFrames": fmo_stats.rx_frames.load(std::sync::atomic::Ordering::Relaxed),
                        }),
                    );
                }
            });
        }));

        fmo.install_raw_handler();
        fmo.install_qso_jump_hook();
        fmo.ensure_aprs_task();
        fmo.start_identity_watchdog();
        fmo.auto_connect_aprs().await;
        fmo.select_default_server().await;
        // 启动即用上次成功的认证自动连接 FMO MQTT（音频引擎先启动，与手动连接路径一致）
        if fmo.mqtt_autoconnect_ready().await {
            self.ensure_audio_running().await;
            (fmo.emit)(serde_json::json!({"type": "log", "level": "info",
                "msg": "检测到证书与上次选定的服务器，自动连接 FMO MQTT"}));
            if let Err(err) = fmo.connect_mqtt(false).await {
                eprintln!("[FMO] 启动自动连接 MQTT 失败: {err}");
            }
        }

        let mut guard = self.fmo.write().await;
        *guard = Some(fmo);
    }

    pub async fn fmo_state(&self) -> Option<Arc<FmoState>> {
        self.fmo.read().await.clone()
    }

    pub async fn apply_fmo_mqtt_state(&self, state: &str, detail: &str) {
        {
            let mut guard = self.inner.write().await;
            // FMO 连接状态独立于 NRL（snapshot.connection 只代表 NRL），
            // FMO 用前端 fmo.state.mqttState 显示，这里不覆盖 NRL connection。
            guard.snapshot.last_text_message = format!("FMO MQTT: {detail}");
            if state == "connected" {
                guard.push_event("FMO MQTT 已连接", detail, "accent");
            } else if state == "error" {
                guard.push_event("FMO MQTT 错误", detail, "warn");
            } else if state == "connecting" {
                guard.push_event("FMO MQTT 连接中", detail, "info");
            } else {
                guard.push_event("FMO MQTT 已断开", detail, "info");
            }
        }
        if let Some(app) = self.app.read().await.clone() {
            let _ = app.emit("runtime://snapshot", self.snapshot().await);
        }
    }

    pub async fn bootstrap(&self) -> RuntimeBootstrap {
        let (snapshot, presence, timeline) = {
            let guard = self.inner.read().await;
            (
                self.snapshot_locked(&guard),
                guard.presence.clone(),
                guard.timeline.clone(),
            )
        };
        RuntimeBootstrap {
            snapshot,
            presence,
            timeline,
            serial_tunnel: self.serial_tunnel.snapshot().await,
        }
    }

    pub async fn snapshot(&self) -> SessionSnapshot {
        let mut snap = self.inner.read().await.snapshot.clone();
        // 链路活跃时间存在原子里（避免每报文抢写锁），emit 时注入
        snap.nrl_last_rx_ms = self.nrl_last_rx_ms.load(Ordering::Relaxed);
        snap
    }

    /// 克隆 guard.snapshot 并注入链路活跃时间。
    /// 该字段只存在原子里，直接 clone 会丢成 0，前端合并后 NRL 链路状态闪"离线"，
    /// PTT 按钮在按下瞬间出现禁止态。所有写锁内的返回路径统一走这里。
    fn snapshot_locked(&self, guard: &RuntimeData) -> SessionSnapshot {
        let mut snap = guard.snapshot.clone();
        snap.nrl_last_rx_ms = self.nrl_last_rx_ms.load(Ordering::Relaxed);
        snap
    }

    pub async fn apply_config(&self, config: RuntimeConfig) {
        // 同步 FMO 独立呼号配置
        let fmo_callsign = config.fmo_callsign.clone();
        if let Some(fmo) = self.fmo_state().await {
            *fmo.configured_callsign.lock().unwrap() = fmo_callsign;
        }
        let mut guard = self.inner.write().await;
        guard.config = config.clone();
        guard.snapshot.callsign = config.callsign;
        guard.snapshot.ssid = config.ssid;
        guard.snapshot.room_name = config.room_name;
    }

    pub async fn realtime_audio_state(&self) -> RealtimeAudioState {
        let guard = self.inner.read().await;
        RealtimeAudioState {
            active_speaker: guard.snapshot.active_speaker.clone(),
            active_speaker_ssid: guard.snapshot.active_speaker_ssid,
            rx_level: guard.snapshot.rx_level,
            tx_level: guard.snapshot.tx_level,
            rx_spectrum: guard.snapshot.rx_spectrum.clone(),
            tx_spectrum: guard.snapshot.tx_spectrum.clone(),
            queued_frames: guard.snapshot.queued_frames,
            uplink_kbps: guard.snapshot.uplink_kbps,
            downlink_kbps: guard.snapshot.downlink_kbps,
            is_transmitting: guard.snapshot.is_transmitting,
            rx_codec: self.rx_codec.lock().map(|c| c.clone()).unwrap_or_default(),
            rx_seq: self.rx_voice_seq.load(Ordering::Relaxed),
        }
    }

    /// 限速 snapshot emit：低频整包同步，避免高频语音状态拖慢前端整页渲染
    pub async fn throttled_emit_snapshot(&self, app: &AppHandle) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let last = self.last_snapshot_emit_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last) < 250 {
            return;
        }
        self.last_snapshot_emit_ms.store(now_ms, Ordering::Relaxed);
        let _ = app.emit("runtime://snapshot", self.snapshot().await);
    }

    /// 高频音频状态走轻量事件，减少主界面在收音期间的整包重渲染。
    /// 限速 16ms：NRL 语音 20ms 一帧，40ms 限速会丢掉一半帧的频谱数据，波形肉眼可见地卡。
    pub async fn throttled_emit_audio_state(&self, app: &AppHandle) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let last = self.last_audio_emit_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last) < 16 {
            return;
        }
        self.last_audio_emit_ms.store(now_ms, Ordering::Relaxed);
        let _ = app.emit("runtime://audio-state", self.realtime_audio_state().await);
    }

    pub async fn connect(&self, config: RuntimeConfig) -> SessionSnapshot {
        let maybe_app = self.app.read().await.clone();
        let is_fmo = config.protocol == "fmo";
        {
            let mut guard = self.inner.write().await;
            guard.config = config.clone();
            guard.snapshot.connection = "connecting".into();
            guard.snapshot.callsign = config.callsign.clone();
            guard.snapshot.ssid = config.ssid;
            guard.snapshot.room_name = config.room_name.clone();
            guard.snapshot.active_speaker.clear();
            guard.snapshot.active_speaker_ssid = 0;
            guard.snapshot.packet_loss = 0.0;
            guard.snapshot.latency_ms = 0;
            guard.snapshot.jitter_ms = 0;
            guard.snapshot.uplink_kbps = 0.0;
            guard.snapshot.downlink_kbps = 0.0;
            guard.snapshot.rx_level = 0.0;
            guard.snapshot.tx_level = 0.0;
            guard.snapshot.rx_spectrum.fill(0.0);
            guard.snapshot.tx_spectrum.fill(0.0);
            guard.snapshot.queued_frames = 0;
            guard.snapshot.last_text_message = if is_fmo {
                "正在等待 FMO MQTT 连接".into()
            } else {
                "正在等待服务器心跳确认".into()
            };
            guard.last_heartbeat_at = None;
            guard.heartbeat_timeout_reported = false;
            guard.last_heartbeat_sent_at = None;
            guard.latency_ewma_ms = None;
            guard.last_voice_arrival_at = None;
            guard.jitter_ewma_ms = None;
            guard.presence.clear();
            guard.push_presence(&config.callsign, config.ssid, "本机", "online");
            guard.tx_protocol = if is_fmo { "fmo".into() } else { "nrl".into() };
            if is_fmo {
                guard.push_event("开始连接", "已发起 FMO MQTT 会话，等待服务器确认", "accent");
            } else {
                guard.push_event(
                    "开始连接",
                    "已发起 NRL UDP 会话，等待服务器心跳确认",
                    "accent",
                );
            }
        }
        if let Some(app) = maybe_app {
            if is_fmo {
                if let Some(fmo) = self.fmo_state().await {
                    if let Err(err) = fmo.connect_mqtt(false).await {
                        let mut guard = self.inner.write().await;
                        guard.snapshot.connection = "disconnected".into();
                        guard.snapshot.last_text_message = format!("FMO 连接失败: {err}");
                        guard.push_event("FMO 连接失败", &err, "warn");
                        return self.snapshot_locked(&guard);
                    }
                } else {
                    let mut guard = self.inner.write().await;
                    guard.snapshot.connection = "disconnected".into();
                    guard.snapshot.last_text_message = "FMO 状态未初始化".into();
                    guard.push_event("FMO 初始化失败", "请重新启动应用", "warn");
                    return self.snapshot_locked(&guard);
                }
            } else if let Err(err) = self.udp.connect(app, self.clone(), config.clone()).await {
                let mut guard = self.inner.write().await;
                guard.snapshot.connection = "disconnected".into();
                guard.snapshot.last_text_message = format!("连接失败: {err}");
                guard.push_event("连接失败", &err, "warn");
                return self.snapshot_locked(&guard);
            }
        }
        self.ensure_audio_started().await;
        self.spawn_capture_forwarder();
        self.spawn_voice_session_watchdog();
        if !is_fmo {
            self.spawn_heartbeat_watchdog();
        }
        self.snapshot().await
    }

    /// 确保音频引擎与采集已启动（FMO 独立连接时调用，不依赖 config.protocol）。
    pub async fn ensure_audio_running(&self) -> SessionSnapshot {
        self.ensure_audio_started().await;
        self.spawn_capture_forwarder();
        self.spawn_voice_session_watchdog();
        self.snapshot().await
    }

    pub async fn disconnect(&self) -> SessionSnapshot {
        let is_fmo = self.inner.read().await.config.protocol == "fmo";
        if is_fmo {
            if let Some(fmo) = self.fmo_state().await {
                fmo.stop_tx().await;
                fmo.disconnect_mqtt().await;
            }
        } else {
            self.udp.disconnect().await;
        }
        // 双协议共存：断开时不停止音频引擎与采集，保证另一协议仍可 PTT/收声
        self.heartbeat_watchdog_running
            .store(false, Ordering::Relaxed);
        if let Ok(mut tx) = self.nrl_opus_tx.lock() {
            if let Some(e) = tx.as_mut() {
                e.reset();
            }
        }
        if let Ok(mut rx) = self.nrl_opus_rx.lock() {
            *rx = None;
        }
        let mut guard = self.inner.write().await;
        guard.snapshot.connection = "disconnected".into();
        guard.nrl_connected = false;
        guard.snapshot.is_transmitting = false;
        guard.snapshot.rx_level = 0.0;
        guard.snapshot.tx_level = 0.0;
        guard.snapshot.rx_spectrum.fill(0.0);
        guard.snapshot.tx_spectrum.fill(0.0);
        guard.snapshot.queued_frames = 0;
        guard.snapshot.last_text_message = "会话已断开".into();
        guard.snapshot.active_speaker.clear();
        guard.snapshot.active_speaker_ssid = 0;
        guard.voice_session_nrl = None;
        guard.voice_session_fmo = None;
        guard.last_heartbeat_at = None;
        guard.heartbeat_timeout_reported = false;
        guard.last_heartbeat_sent_at = None;
        guard.latency_ewma_ms = None;
        guard.last_voice_arrival_at = None;
        guard.jitter_ewma_ms = None;
        guard.presence.clear();
        guard.push_event("链路断开", "用户主动断开当前房间连接", "warn");
        self.snapshot_locked(&guard)
    }

    pub async fn toggle_transmit(&self) -> SessionSnapshot {
        let enabled = !self.inner.read().await.snapshot.is_transmitting;
        self.set_transmit(enabled).await
    }

    pub async fn set_transmit(&self, enabled: bool) -> SessionSnapshot {
        let proto = self.inner.read().await.tx_protocol.clone();
        self.set_transmit_proto(&proto, enabled).await
    }

    /// 按指定协议发射（NRL/FMO 各自独立 PTT）。
    pub async fn set_transmit_proto(&self, protocol: &str, enabled: bool) -> SessionSnapshot {
        let mut guard = self.inner.write().await;
        if guard.snapshot.is_transmitting == enabled {
            return self.snapshot_locked(&guard);
        }
        guard.snapshot.is_transmitting = enabled;
        let is_fmo = protocol == "fmo";
        self.audio.set_transmitting(enabled);

        let detail = if enabled {
            guard.outgoing_voice_data.clear();
            guard.outgoing_voice_start = Some(Instant::now());
            if is_fmo {
                let mode = guard.config.fmo_voice_mode.clone();
                drop(guard);
                let fmo = self.fmo_state().await;
                if let Some(fmo) = fmo {
                    if let Err(err) = fmo.start_tx(&mode).await {
                        let mut guard = self.inner.write().await;
                        guard.snapshot.is_transmitting = false;
                        self.audio.set_transmitting(false);
                        guard.push_event("FMO 发射失败", &err, "warn");
                        return self.snapshot_locked(&guard);
                    }
                }
                let mut guard = self.inner.write().await;
                let mode_label = if mode == "opus" { "Opus" } else { "IMA ADPCM" };
                guard.push_event(
                    "发射切换",
                    &format!("FMO 发射链路进入发送状态（{mode_label}）"),
                    "accent",
                );
                return self.snapshot_locked(&guard);
            }
            "NRL 发射链路进入发送状态，等待真实麦克风与编码器挂接"
        } else {
            guard.snapshot.tx_level = 0.0;
            guard.snapshot.tx_spectrum.fill(0.0);
            guard.snapshot.uplink_kbps = 0.0;
            let duration_ms = guard
                .outgoing_voice_start
                .map(|s| s.elapsed().as_millis())
                .unwrap_or(0);
            let voice_data = guard.outgoing_voice_data.clone();
            guard.outgoing_voice_data.clear();
            guard.outgoing_voice_start = None;
            if is_fmo {
                drop(guard);
                if let Some(fmo) = self.fmo_state().await {
                    fmo.stop_tx().await;
                }
                if !voice_data.is_empty() && duration_ms > 100 {
                    let data_copy = voice_data.clone();
                    let dur_copy = duration_ms;
                    self.emit_outgoing_voice_message_with_data(data_copy, dur_copy, "FMO")
                        .await;
                }
                let mut guard = self.inner.write().await;
                guard.push_event("发射切换", "FMO 发射结束，返回监听模式", "accent");
                return self.snapshot_locked(&guard);
            }
            if !voice_data.is_empty() && duration_ms > 100 {
                let data_copy = voice_data.clone();
                let dur_copy = duration_ms;
                drop(guard);
                self.emit_outgoing_voice_message_with_data(data_copy, dur_copy, "NRL")
                    .await;
                let mut guard = self.inner.write().await;
                guard.push_event("发射切换", "NRL 发射结束，返回监听模式", "accent");
                return self.snapshot_locked(&guard);
            }
            "NRL 发射结束，返回监听模式"
        };
        guard.push_event("发射切换", detail, "accent");
        self.snapshot_locked(&guard)
    }

    /// 设置当前发射协议（NRL/FMO 各自 PTT 前调用）。
    pub async fn set_tx_protocol(&self, protocol: &str) {
        let mut guard = self.inner.write().await;
        if protocol == "fmo" {
            guard.tx_protocol = "fmo".into();
        } else {
            guard.tx_protocol = "nrl".into();
        }
    }

    pub async fn toggle_monitor(&self) -> SessionSnapshot {
        let mut guard = self.inner.write().await;
        guard.snapshot.is_monitoring = !guard.snapshot.is_monitoring;
        self.audio.set_monitoring(guard.snapshot.is_monitoring);
        let detail = if guard.snapshot.is_monitoring {
            "监听链路已开启"
        } else {
            "监听链路已关闭"
        };
        guard.push_event("监听切换", detail, "info");
        self.snapshot_locked(&guard)
    }

    pub async fn update_jitter_buffer(&self, value: u32) -> SessionSnapshot {
        let mut guard = self.inner.write().await;
        guard.snapshot.devices.jitter_buffer_ms = value;
        guard.push_event(
            "调整缓冲",
            &format!("抖动缓冲更新为 {value}ms，后续会驱动真实 jitter buffer"),
            "accent",
        );
        self.snapshot_locked(&guard)
    }

    pub async fn send_text_message(
        &self,
        config: &RuntimeConfig,
        message: String,
    ) -> SessionSnapshot {
        if config.protocol == "fmo" {
            // FMO 无统一文本通道：仅在本地记录
            {
                let mut guard = self.inner.write().await;
                guard.snapshot.last_text_message = "FMO 文本消息（本地记录）".into();
                guard.push_event("FMO 文本", &message, "info");
            }
            self.emit_chat_message("self", &message, &config.callsign, config.ssid)
                .await;
            return self.snapshot().await;
        }
        let _ = self.udp.send_text(config, &message).await;
        {
            let mut guard = self.inner.write().await;
            guard.snapshot.last_text_message = "文本消息已发送".into();
            guard.push_event("文本消息", &message, "info");
        }
        self.emit_chat_message("self", &message, &config.callsign, config.ssid)
            .await;
        self.snapshot().await
    }

    pub async fn save_config_snapshot(&self, config: &RuntimeConfig) -> SessionSnapshot {
        let mut guard = self.inner.write().await;
        guard.config = config.clone();
        guard.snapshot.callsign = config.callsign.clone();
        guard.snapshot.ssid = config.ssid;
        guard.snapshot.room_name = config.room_name.clone();
        guard.push_event(
            "配置已保存",
            &format!(
                "{}:{} / {}-{}",
                config.server, config.port, config.callsign, config.ssid
            ),
            "accent",
        );
        self.snapshot_locked(&guard)
    }

    pub async fn udp_send_at_state(
        &self,
        config: &RuntimeConfig,
        lines: &[String],
    ) -> Result<(), String> {
        self.udp.send_at_state(config, lines).await
    }

    pub async fn serial_tunnel_snapshot(&self) -> SerialTunnelSnapshot {
        self.serial_tunnel.snapshot().await
    }

    async fn emit_serial_tunnel_snapshot(&self) {
        if let Some(app) = self.app.read().await.clone() {
            let _ = app.emit(
                "runtime://serial-tunnel",
                self.serial_tunnel.snapshot().await,
            );
        }
    }

    pub async fn start_serial_tunnel(
        &self,
        config: SerialTunnelConfig,
    ) -> Result<SerialTunnelSnapshot, String> {
        let (read_tx, mut read_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let snapshot = self.serial_tunnel.start(config.clone(), read_tx).await?;
        {
            let mut guard = self.inner.write().await;
            guard.config.serial_tunnel = config;
            guard.push_event("串口透传", "物理/虚拟串口桥接已启动", "accent");
        }
        let runtime = self.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(data) = read_rx.recv().await {
                let config = runtime.current_config().await;
                match runtime.udp.send_serial_tunnel(&config, data).await {
                    Ok(()) => {
                        runtime.emit_serial_tunnel_snapshot().await;
                    }
                    Err(err) => {
                        runtime
                            .push_runtime_event("串口透传发送失败", &err, "warn")
                            .await;
                    }
                }
            }
        });
        Ok(snapshot)
    }

    pub async fn stop_serial_tunnel(&self) -> SerialTunnelSnapshot {
        let snapshot = self.serial_tunnel.stop().await;
        {
            let mut guard = self.inner.write().await;
            guard.push_event("串口透传", "串口透传已关闭", "info");
        }
        snapshot
    }

    pub async fn write_serial_tunnel_data(&self, data: &[u8]) {
        match self.serial_tunnel.write(data).await {
            Ok(()) => {
                self.emit_serial_tunnel_snapshot().await;
            }
            Err(err) => {
                self.push_runtime_event("串口透传写入失败", &err, "warn")
                    .await;
            }
        }
    }

    pub async fn current_config(&self) -> RuntimeConfig {
        self.inner.read().await.config.clone()
    }

    pub async fn at_state_lines(&self) -> Vec<String> {
        let guard = self.inner.read().await;
        let duck_mic = if guard.at_state.duck_mic { "ON" } else { "OFF" };
        let duck_music = if guard.at_state.duck_music {
            "ON"
        } else {
            "OFF"
        };
        vec![
            "AT+PLAY_ID=1".into(),
            "AT+PREW=1".into(),
            "AT+NEXT=1".into(),
            "AT+PAUSE=1".into(),
            format!("AT+VOLUME={}", guard.at_state.volume),
            format!("AT+DUCK_MIC={duck_mic}"),
            format!("AT+DUCK_MUSIC={duck_music}"),
            format!("AT+DUCK_SCALE={}", guard.at_state.duck_scale),
        ]
    }

    pub async fn push_runtime_event(&self, title: &str, detail: &str, tone: &str) {
        let event = {
            let mut guard = self.inner.write().await;
            guard.push_event(title, detail, tone)
        };
        let has_app = self.app.read().await.is_some();
        eprintln!("[Runtime] push_runtime_event: title={title:?} has_app={has_app}");
        if let Some(app) = self.app.read().await.clone() {
            let r1 = app.emit("runtime://timeline", &event);
            let r2 = app.emit("runtime://snapshot", self.snapshot().await);
            eprintln!("[Runtime] emit timeline={r1:?} snapshot={r2:?}");
        }
    }

    pub async fn note_voice_frame(
        &self,
        callsign: String,
        ssid: u8,
        samples: usize,
        level: f32,
        spectrum: &[f32],
        pcm_data: &[i16],
    ) {
        self.note_voice_frame_impl(callsign, ssid, samples, level, spectrum, pcm_data, true, "nrl")
            .await
    }

    /// FMO 专用：录音与会话逻辑与 NRL 一致，但不更新共享显示字段
    /// （rx_spectrum/rx_level/active_speaker 等保持 NRL 独立，FMO 用自己的 fmo.stats）。
    pub async fn note_fmo_voice_frame(
        &self,
        callsign: String,
        samples: usize,
        level: f32,
        spectrum: &[f32],
        pcm_data: &[i16],
    ) {
        self.note_voice_frame_impl(callsign, 0, samples, level, spectrum, pcm_data, false, "fmo")
            .await
    }

    async fn note_voice_frame_impl(
        &self,
        callsign: String,
        ssid: u8,
        samples: usize,
        level: f32,
        spectrum: &[f32],
        pcm_data: &[i16],
        update_display: bool,
        protocol: &'static str,
    ) {
        let now = Instant::now();
        let emitted = Vec::new();
        // 在持有写锁期间收集需要 emit 的旧会话，锁释放后再发送
        // 不能在写锁内调用 emit_voice_message：该函数会再次获取 inner.read()，同一 task
        // 内持有写锁再等读锁会永久死锁，导致所有后续锁操作（包括群组切换的 save_config）卡住
        let finished_session: Option<(VoiceSession, u128)> = {
            let mut guard = self.inner.write().await;
            if update_display {
                // RFC 3550 风格的抖动估算：|到达间隔 - 期望间隔| 的 EWMA，期望间隔=20ms（160 样本 @ 8kHz）
                if let Some(prev) = guard.last_voice_arrival_at {
                    let delta_ms = now.saturating_duration_since(prev).as_secs_f32() * 1000.0;
                    let expected_ms = (samples as f32 / 8.0).max(1.0);
                    let dev = (delta_ms - expected_ms).abs();
                    let next = match guard.jitter_ewma_ms {
                        Some(prev) => prev + (dev - prev) / 16.0,
                        None => dev,
                    };
                    guard.jitter_ewma_ms = Some(next);
                    guard.snapshot.jitter_ms = next.round().max(0.0) as u32;
                }
                guard.last_voice_arrival_at = Some(now);
                guard.snapshot.active_speaker = callsign.clone();
                guard.snapshot.active_speaker_ssid = ssid;
                guard.snapshot.rx_level = level;
                guard.snapshot.rx_spectrum = spectrum.to_vec();
                guard.snapshot.queued_frames = (samples / 160).max(1) as u32;
                guard.snapshot.downlink_kbps = packet_kbps(samples / 2);
                guard.push_presence(&callsign, ssid, "远端节点", "speaking");
            }

            // 按协议选槽：NRL/FMO 各自独立，同协议不同说话人才顶会话
            let slot = if protocol == "fmo" {
                &mut guard.voice_session_fmo
            } else {
                &mut guard.voice_session_nrl
            };
            match slot.take() {
                Some(mut session) if session.callsign == callsign && session.ssid == ssid => {
                    session.last_seen_at = now;
                    let next_len = session.audio_data.len().saturating_add(pcm_data.len());
                    if next_len <= MAX_VOICE_SESSION_SAMPLES {
                        session.audio_data.extend_from_slice(pcm_data);
                        *slot = Some(session);
                        None
                    } else {
                        let remaining =
                            MAX_VOICE_SESSION_SAMPLES.saturating_sub(session.audio_data.len());
                        if remaining > 0 {
                            session.audio_data.extend_from_slice(&pcm_data[..remaining]);
                        }
                        let elapsed = now.duration_since(session.started_at).as_millis();
                        let mut new_session = VoiceSession {
                            callsign: callsign.clone(),
                            ssid,
                            protocol,
                            started_at: now,
                            last_seen_at: now,
                            audio_data: Vec::new(),
                        };
                        if remaining < pcm_data.len() {
                            new_session
                                .audio_data
                                .extend_from_slice(&pcm_data[remaining..]);
                        }
                        *slot = Some(new_session);
                        Some((session, elapsed))
                    }
                }
                Some(session) => {
                    let elapsed = now.duration_since(session.started_at).as_millis();
                    let mut new_session = VoiceSession {
                        callsign: callsign.clone(),
                        ssid,
                        protocol,
                        started_at: now,
                        last_seen_at: now,
                        audio_data: Vec::new(),
                    };
                    new_session.audio_data.extend_from_slice(pcm_data);
                    *slot = Some(new_session);
                    Some((session, elapsed))
                }
                None => {
                    let mut new_session = VoiceSession {
                        callsign: callsign.clone(),
                        ssid,
                        protocol,
                        started_at: now,
                        last_seen_at: now,
                        audio_data: Vec::new(),
                    };
                    new_session.audio_data.extend_from_slice(pcm_data);
                    *slot = Some(new_session);
                    None
                }
            }
        }; // 写锁在此处释放
           // 锁释放后再 emit，避免 emit_voice_message 内的 inner.read() 与写锁死锁
        if let Some((session, elapsed)) = finished_session {
            self.emit_voice_message(&session, elapsed).await;
        }
        self.emit_runtime_updates(emitted).await;
    }

    pub async fn note_transmit_frame(&self, samples: usize, level: f32, spectrum: &[f32]) {
        {
            let mut guard = self.inner.write().await;
            if !guard.snapshot.is_transmitting {
                return;
            }
            guard.snapshot.tx_level = level;
            guard.snapshot.tx_spectrum = spectrum.to_vec();
            guard.snapshot.uplink_kbps = packet_kbps(samples / 2);
        }
        if let Some(app) = self.app.read().await.clone() {
            self.throttled_emit_audio_state(&app).await;
        }
    }

    pub fn enqueue_received_pcm(&self, pcm: &[i16], channel: RxChannel) {
        self.audio.enqueue_received_pcm(pcm, channel);
    }

    pub async fn note_remote_activity(&self, callsign: &str, ssid: u8, state: &str) {
        let mut guard = self.inner.write().await;
        guard.push_presence(callsign, ssid, "远端节点", state);
    }

    pub async fn note_heartbeat_sent(&self) {
        let mut guard = self.inner.write().await;
        if guard.last_heartbeat_sent_at.is_none() {
            guard.last_heartbeat_sent_at = Some(Instant::now());
        }
    }

    pub async fn note_heartbeat(&self, callsign: &str, ssid: u8) {
        let mut emitted = Vec::new();
        {
            let mut guard = self.inner.write().await;
            let first_confirm = guard.last_heartbeat_at.is_none();
            let recovered = guard.heartbeat_timeout_reported;
            let now = Instant::now();
            if let Some(sent_at) = guard.last_heartbeat_sent_at.take() {
                let rtt_ms = now.saturating_duration_since(sent_at).as_secs_f32() * 1000.0;
                let next = match guard.latency_ewma_ms {
                    Some(prev) => prev + (rtt_ms - prev) * 0.2,
                    None => rtt_ms,
                };
                guard.latency_ewma_ms = Some(next);
                guard.snapshot.latency_ms = next.round().max(0.0) as u32;
            }
            guard.push_presence(callsign, ssid, "远端节点", "online");
            guard.nrl_connected = true;
            guard.snapshot.connection = "connected".into();
            guard.snapshot.last_text_message = format!("收到 {}-{} 心跳确认", callsign, ssid);
            guard.last_heartbeat_at = Some(now);
            guard.heartbeat_timeout_reported = false;
            if first_confirm {
                emitted.push(guard.push_event(
                    "心跳已建立",
                    &format!("收到 {}-{} 首次心跳确认", callsign, ssid),
                    "info",
                ));
            } else if recovered {
                emitted.push(guard.push_event(
                    "心跳恢复",
                    &format!("{}-{} 心跳恢复正常", callsign, ssid),
                    "accent",
                ));
            }
        }
        self.emit_runtime_updates(emitted).await;
    }

    pub async fn apply_text_message(&self, text: &str) {
        let mut guard = self.inner.write().await;
        guard.snapshot.last_text_message = text.into();
    }

    pub async fn note_text_message(&self, text: &str, callsign: &str, ssid: u8) {
        self.emit_chat_message("remote", text, callsign, ssid).await;
    }

    pub async fn apply_at_command(&self, command: &str, value: &str) {
        let mut guard = self.inner.write().await;
        guard.at_state.last_command = format!("{command}={value}");
        match command {
            "AT+VOLUME" => {
                if let Ok(parsed) = value.parse::<u8>() {
                    guard.at_state.volume = parsed.min(100);
                }
            }
            "AT+DUCK_MIC" => {
                guard.at_state.duck_mic = value.eq_ignore_ascii_case("ON");
            }
            "AT+DUCK_MUSIC" => {
                guard.at_state.duck_music = value.eq_ignore_ascii_case("ON");
            }
            "AT+DUCK_SCALE" => {
                if let Ok(parsed) = value.parse::<u8>() {
                    guard.at_state.duck_scale = parsed.min(100);
                }
            }
            _ => {}
        }
    }

    async fn ensure_audio_started(&self) {
        eprintln!("[Runtime] ensure_audio_started: calling audio.start()");
        match self.audio.start() {
            Ok((devices, logs)) => {
                eprintln!(
                    "[Runtime] audio.start() OK: input={} output={}",
                    devices.input_device, devices.output_device
                );
                {
                    let mut guard = self.inner.write().await;
                    guard.snapshot.devices = devices.clone();
                }
                self.audio.set_monitoring(true);
                for line in &logs {
                    self.push_runtime_event("音频设备", line, "info").await;
                }
                let input_mode = if devices.input_resampling {
                    format!("{}Hz→8000Hz重采样", devices.input_device_rate)
                } else {
                    "8000Hz直通".to_string()
                };
                let output_mode = if devices.output_resampling {
                    format!("8000Hz→{}Hz重采样", devices.output_device_rate)
                } else {
                    "8000Hz直通".to_string()
                };
                self.push_runtime_event(
                    "音频链已启动",
                    &format!(
                        "输入: {}({}) / 输出: {}({})",
                        devices.input_device, input_mode, devices.output_device, output_mode
                    ),
                    "accent",
                )
                .await;
            }
            Err(err) => {
                eprintln!("[Runtime] audio.start() FAILED: {err}");
                self.push_runtime_event("音频启动失败", &err, "warn").await;
            }
        }
    }

    fn spawn_capture_forwarder(&self) {
        if self.capture_task_running.swap(true, Ordering::Relaxed) {
            return;
        }
        let Some(mut rx) = self.audio.take_capture_receiver() else {
            self.capture_task_running.store(false, Ordering::Relaxed);
            return;
        };
        let runtime = self.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(frame) = rx.recv().await {
                let config = runtime.current_config().await;
                let tx_proto = runtime.inner.read().await.tx_protocol.clone();
                let analysis = analyze_pcm_frame(&frame);
                runtime
                    .note_transmit_frame(frame.len(), analysis.level, &analysis.spectrum)
                    .await;
                let is_transmitting = runtime.inner.read().await.snapshot.is_transmitting;
                if is_transmitting {
                    let mut guard = runtime.inner.write().await;
                    guard.outgoing_voice_data.extend_from_slice(&frame);
                }
                if tx_proto == "fmo" {
                    if is_transmitting {
                        if let Some(fmo) = runtime.fmo_state().await {
                            fmo.feed_pcm(&frame).await;
                        }
                    }
                } else if config.voice_codec == "opus" {
                    runtime.send_nrl_opus_voice(&config, &frame).await;
                } else {
                    let _ = runtime.udp.send_voice_frame(&config, &frame).await;
                }
            }
            runtime.capture_task_running.store(false, Ordering::Relaxed);
        });
    }

    /// NRL type=8 Opus 接收解码：16k 解码 → 8k 下采样 → i16。
    pub async fn decode_nrl_opus_frame(&self, packet: &[u8]) -> Option<Vec<i16>> {
        let mut init_err: Option<String> = None;
        let decoded = {
            let mut decoder = self.nrl_opus_rx.lock().unwrap();
            if decoder.is_none() {
                match NrlOpusDecoder::new() {
                    Ok(d) => {
                        *decoder = Some(d);
                        decoder.as_mut().unwrap().decode(packet).ok()
                    }
                    Err(err) => {
                        init_err = Some(err);
                        None
                    }
                }
            } else {
                decoder.as_mut().unwrap().decode(packet).ok()
            }
        };
        if let Some(err) = init_err {
            self.push_runtime_event("Opus 解码器初始化失败", &err, "warn")
                .await;
            return None;
        }
        decoded.map(|pcm16| crate::opus::resample_16k_to_8k(&pcm16))
    }

    /// NRL type=8 Opus 发射：8k PCM → 16k 上采样 → Opus 包 → UDP。
    async fn send_nrl_opus_voice(&self, config: &RuntimeConfig, frame: &[i16]) {
        let mut init_err: Option<String> = None;
        let packets: Vec<Vec<u8>> = {
            let mut encoder = self.nrl_opus_tx.lock().unwrap();
            if encoder.is_none() {
                match NrlOpusEncoder::new() {
                    Ok(e) => {
                        *encoder = Some(e);
                        match encoder.as_mut().unwrap().feed(frame) {
                            Ok(pkts) => pkts,
                            Err(err) => {
                                init_err = Some(err);
                                Vec::new()
                            }
                        }
                    }
                    Err(err) => {
                        init_err = Some(err);
                        Vec::new()
                    }
                }
            } else {
                match encoder.as_mut().unwrap().feed(frame) {
                    Ok(pkts) => pkts,
                    Err(err) => {
                        init_err = Some(err);
                        Vec::new()
                    }
                }
            }
        };
        if let Some(err) = init_err {
            self.push_runtime_event("Opus 编码失败", &err, "warn").await;
            return;
        }
        for pkt in packets {
            let _ = self.udp.send_voice_frame_opus(config, &pkt).await;
        }
    }

    fn spawn_heartbeat_watchdog(&self) {
        if self
            .heartbeat_watchdog_running
            .swap(true, Ordering::Relaxed)
        {
            return;
        }
        let runtime = self.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let mut emitted = Vec::new();
                {
                    let mut guard = runtime.inner.write().await;
                    if guard.snapshot.connection == "disconnected" {
                        break;
                    }
                    let Some(last_heartbeat_at) = guard.last_heartbeat_at else {
                        continue;
                    };
                    if guard.heartbeat_timeout_reported
                        || last_heartbeat_at.elapsed() < Duration::from_secs(6)
                    {
                        continue;
                    }
                    guard.heartbeat_timeout_reported = true;
                    guard.nrl_connected = false;
                    guard.snapshot.connection = "disconnected".into();
                    guard.snapshot.is_transmitting = false;
                    guard.snapshot.last_text_message = "服务器心跳超时，链路已断开".into();
                    emitted.push(guard.push_event(
                        "心跳超时",
                        "超过 6 秒未收到服务器心跳确认，已停止等待且不自动重连",
                        "warn",
                    ));
                }
                runtime.udp.disconnect().await;
                runtime.audio.set_transmitting(false);
                runtime.emit_runtime_updates(emitted).await;
            }
            runtime
                .heartbeat_watchdog_running
                .store(false, Ordering::Relaxed);
        });
    }

    fn spawn_voice_session_watchdog(&self) {
        if self.voice_watchdog_running.swap(true, Ordering::Relaxed) {
            return;
        }
        let runtime = self.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let emitted = Vec::new();
                let session_info = {
                    let mut guard = runtime.inner.write().await;
                    if guard.snapshot.connection == "disconnected" {
                        break;
                    }
                    // NRL/FMO 两个槽各自检查超时（1s 无新帧视为发言结束）
                    let mut finished = Vec::new();
                    {
                        // 解构出两个字段的可变引用（MutexGuard 不支持两次 DerefMut 借用它）
                        let RuntimeData {
                            voice_session_nrl,
                            voice_session_fmo,
                            ..
                        } = &mut *guard;
                        for slot in [voice_session_nrl, voice_session_fmo] {
                            let Some(session) = slot.take() else {
                                continue;
                            };
                            if session.last_seen_at.elapsed() < Duration::from_secs(1) {
                                *slot = Some(session);
                                continue;
                            }
                            let elapsed = session
                                .last_seen_at
                                .duration_since(session.started_at)
                                .as_millis();
                            finished.push((session, elapsed));
                        }
                    }
                    // 只有 NRL 会话结束时才清 NRL 的显示状态（FMO 有独立统计）
                    if finished.iter().any(|(s, _)| s.protocol == "nrl") {
                        guard.snapshot.rx_level = 0.0;
                        guard.snapshot.rx_spectrum.fill(0.0);
                        guard.snapshot.queued_frames = 0;
                        guard.snapshot.downlink_kbps = 0.0;
                        guard.snapshot.active_speaker.clear();
                        guard.snapshot.active_speaker_ssid = 0;
                        guard.last_voice_arrival_at = None;
                    }
                    finished
                };
                if !session_info.is_empty() {
                    for (session, elapsed) in session_info {
                        runtime.emit_voice_message(&session, elapsed).await;
                    }
                    // 语音结束后主动推一次 snapshot，通知前端 rx_level/rx_spectrum 已归零
                    // watchdog 已将这些字段清零（write lock 内），但没有 emit，前端会一直显示旧波形
                    if let Some(app) = runtime.app.read().await.clone() {
                        let _ = app.emit(
                            "runtime://audio-state",
                            runtime.realtime_audio_state().await,
                        );
                        let _ = app.emit("runtime://snapshot", runtime.snapshot().await);
                    }
                }
                runtime.emit_runtime_updates(emitted).await;
            }
            runtime
                .voice_watchdog_running
                .store(false, Ordering::Relaxed);
        });
    }

    async fn emit_runtime_updates(&self, events: Vec<TimelineEvent>) {
        if events.is_empty() {
            return;
        }
        if let Some(app) = self.app.read().await.clone() {
            for event in events {
                let _ = app.emit("runtime://timeline", event);
            }
            let _ = app.emit("runtime://snapshot", self.snapshot().await);
        }
    }

    async fn emit_chat_message(&self, side: &str, text: &str, callsign: &str, ssid: u8) {
        if let Some(app) = self.app.read().await.clone() {
            let id = {
                let mut guard = self.inner.write().await;
                guard.tick += 1;
                guard.tick
            };
            let time = chrono_local_now();
            let event = ChatMessageEvent {
                id: format!("chat-{id}"),
                side: side.into(),
                text: text.into(),
                meta: format!("{callsign}-{ssid}"),
                time,
                type_: None,
                waveform: None,
                file_path: None,
                duration: None,
            };
            let _ = app.emit("runtime://chat-message", event);
        }
    }

    async fn emit_voice_message(&self, session: &VoiceSession, duration_ms: u128) {
        // 过短的碎片会话（<100ms）不产生消息/录音，与发射侧口径一致
        if duration_ms < 100 {
            return;
        }
        let voice_save_path = {
            let guard = self.inner.read().await;
            guard.config.voice_save_path.clone()
        };

        let file_path = match voice_file_path(
            &session.callsign,
            session.ssid,
            duration_ms,
            &voice_save_path,
        ) {
            Ok(path) => Some(path),
            Err(e) => {
                eprintln!("[Runtime] Failed to prepare voice path: {}", e);
                None
            }
        };

        // 先立即 emit chat 消息，保证前端消息出现不被文件写入延迟
        if let Some(app) = self.app.read().await.clone() {
            let id = {
                let mut guard = self.inner.write().await;
                guard.tick += 1;
                guard.tick
            };
            let time = chrono_local_now();
            let duration_sec = duration_ms as f64 / 1000.0;
            let event = ChatMessageEvent {
                id: format!("chat-{id}"),
                side: "remote".into(),
                text: String::new(),
                meta: format!(
                    "{}-{}-{}-{}",
                    session.protocol.to_uppercase(),
                    session.callsign,
                    session.ssid,
                    format!("{:.1}s", duration_sec)
                ),
                time,
                type_: Some("voice".into()),
                waveform: Some(build_waveform_preview(&session.audio_data, 40)),
                file_path: file_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().to_string()),
                duration: Some(duration_sec),
            };
            let _ = app.emit("runtime://chat-message", event);
        }

        // 文件写入放入独立阻塞线程，不占用 tokio worker，不阻塞 UDP reader
        // 避免文件 I/O 期间 UDP 包积压、Tauri snapshot 事件堆积导致前端误判仍在接收
        let callsign = session.callsign.clone();
        let ssid = session.ssid;
        let audio_data_save = session.audio_data.clone();
        if let Some(file_path_save) = file_path {
            tokio::task::spawn_blocking(move || {
                if let Err(e) = save_voice_to_wav(&file_path_save, &audio_data_save) {
                    eprintln!(
                        "[Runtime] Failed to save voice {}-{} to {}: {}",
                        callsign,
                        ssid,
                        file_path_save.display(),
                        e
                    );
                }
            });
        }
        // 不 await：文件写在后台异步完成，调用方立即返回
    }

    async fn emit_outgoing_voice_message_with_data(
        &self,
        audio_data: Vec<i16>,
        duration_ms: u128,
        protocol: &str,
    ) {
        let (callsign, ssid, voice_save_path) = {
            let guard = self.inner.read().await;
            let config = &guard.config;
            (
                config.callsign.clone(),
                config.ssid,
                config.voice_save_path.clone(),
            )
        };

        if audio_data.is_empty() || duration_ms < 100 {
            return;
        }

        let file_path = match voice_file_path(&callsign, ssid, duration_ms, &voice_save_path) {
            Ok(path) => Some(path),
            Err(e) => {
                eprintln!("[Runtime] Failed to prepare outgoing voice path: {}", e);
                None
            }
        };

        // 先 emit chat 消息
        if let Some(app) = self.app.read().await.clone() {
            let id = {
                let mut guard = self.inner.write().await;
                guard.tick += 1;
                guard.tick
            };
            let time = chrono_local_now();
            let duration_sec = duration_ms as f64 / 1000.0;
            let event = ChatMessageEvent {
                id: format!("chat-{id}"),
                side: "self".into(),
                text: String::new(),
                meta: format!(
                    "{}-{}-{}-{}",
                    protocol,
                    callsign,
                    ssid,
                    format!("{:.1}s", duration_sec)
                ),
                time,
                type_: Some("voice".into()),
                waveform: Some(build_waveform_preview(&audio_data, 40)),
                file_path: file_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().to_string()),
                duration: Some(duration_sec),
            };
            let _ = app.emit("runtime://chat-message", event);
        }

        // 文件写入同样放入阻塞线程，不阻塞 set_transmit 及 runAction，防止 busy 卡死
        if let Some(file_path_save) = file_path {
            tokio::task::spawn_blocking(move || {
                if let Err(e) = save_voice_to_wav(&file_path_save, &audio_data) {
                    eprintln!(
                        "[Runtime] Failed to save outgoing voice {}-{} to {}: {}",
                        callsign,
                        ssid,
                        file_path_save.display(),
                        e
                    );
                }
            });
        }
    }
}

impl RuntimeData {
    fn seed() -> Self {
        Self {
            config: RuntimeConfig::default(),
            tx_protocol: "nrl".into(),
            nrl_connected: false,
            snapshot: SessionSnapshot {
                room_name: "NRL East Hub".into(),
                callsign: "B1NRL".into(),
                ssid: 110,
                active_speaker: String::new(),
                active_speaker_ssid: 0,
                connection: "disconnected".into(),
                nrl_connected: false,
                nrl_last_rx_ms: 0,
                packet_loss: 0.0,
                latency_ms: 0,
                jitter_ms: 0,
                uplink_kbps: 0.0,
                downlink_kbps: 0.0,
                rx_level: 0.0,
                tx_level: 0.0,
                rx_spectrum: vec![0.0; SPECTRUM_BANDS],
                tx_spectrum: vec![0.0; SPECTRUM_BANDS],
                is_transmitting: false,
                is_monitoring: true,
                queued_frames: 0,
                last_text_message: "等待连接服务器".into(),
                devices: DeviceSettings {
                    input_device: "Default Microphone".into(),
                    output_device: "Default Speaker".into(),
                    sample_rate: 8_000,
                    input_device_rate: 8_000,
                    output_device_rate: 8_000,
                    input_resampling: false,
                    output_resampling: false,
                    jitter_buffer_ms: 120,
                    agc_enabled: false,
                    noise_suppression: false,
                    aec_enabled: false,
                },
            },
            presence: vec![],
            timeline: vec![],
            at_state: AtState {
                volume: 100,
                duck_mic: false,
                duck_music: false,
                duck_scale: 50,
                last_command: "AT+VOLUME=100".into(),
            },
            voice_session_nrl: None,
            voice_session_fmo: None,
            outgoing_voice_data: vec![],
            outgoing_voice_start: None,
            last_heartbeat_at: None,
            heartbeat_timeout_reported: false,
            last_heartbeat_sent_at: None,
            latency_ewma_ms: None,
            last_voice_arrival_at: None,
            jitter_ewma_ms: None,
            tick: 0,
        }
    }

    fn push_event(&mut self, title: &str, detail: &str, tone: &str) -> TimelineEvent {
        self.tick += 1;
        let event = TimelineEvent {
            id: format!("event-{}", self.tick),
            time: chrono_local_now(),
            title: title.into(),
            detail: detail.into(),
            tone: tone.into(),
        };
        self.timeline.insert(0, event.clone());
        self.timeline.truncate(10);
        event
    }

    fn push_presence(&mut self, callsign: &str, ssid: u8, role: &str, state: &str) {
        let id = format!("{}-{}", callsign.to_lowercase(), ssid);
        if let Some(item) = self.presence.iter_mut().find(|item| item.id == id) {
            item.state = state.into();
            item.signal = -48;
            item.last_seen = "now".into();
            return;
        }

        self.presence.insert(
            0,
            PresenceItem {
                id,
                callsign: callsign.into(),
                ssid,
                role: role.into(),
                state: state.into(),
                signal: -48,
                last_seen: "now".into(),
            },
        );
        self.presence.truncate(24);
    }
}

pub fn manage(app: &mut tauri::App) {
    let state = RuntimeState::new();
    app.manage(state.clone());
    tauri::async_runtime::block_on(state.set_app_handle(app.handle().clone()));

    // FMO 数据目录：app_config_dir/fmo
    let fmo_dir = app
        .path()
        .app_config_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("fmo");
    let fmo = Arc::new(FmoState::new(fmo_dir));
    tauri::async_runtime::block_on(state.attach_fmo(fmo));

    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_title("NRL Pulse");
    }
}

fn packet_kbps(payload_bytes: usize) -> f32 {
    ((payload_bytes * 8) as f32) / 1000.0
}

pub(crate) fn analyze_pcm_frame(samples: &[i16]) -> FrameAnalysis {
    if samples.is_empty() {
        return FrameAnalysis {
            level: 0.0,
            spectrum: [0.0; SPECTRUM_BANDS],
        };
    }

    let mut peak = 0.0_f32;
    let mut rms_acc = 0.0_f32;
    let mut input = vec![Complex32::new(0.0, 0.0); SPECTRUM_FFT_SIZE];
    let sample_count = samples.len().min(SPECTRUM_FFT_SIZE);

    for (i, &sample) in samples.iter().take(sample_count).enumerate() {
        let normalized = sample as f32 / i16::MAX as f32;
        peak = peak.max(normalized.abs());
        rms_acc += normalized * normalized;
        let window = 0.5
            - 0.5
                * ((2.0 * std::f32::consts::PI * i as f32)
                    / (sample_count.saturating_sub(1).max(1) as f32))
                    .cos();
        input[i] = Complex32::new(normalized * window, 0.0);
    }

    spectrum_fft().process(&mut input);

    let bin_hz = SPECTRUM_SAMPLE_RATE / SPECTRUM_FFT_SIZE as f32;
    let max_bin = SPECTRUM_FFT_SIZE / 2;
    let mut magnitudes = vec![0.0_f32; max_bin + 1];
    let mut global_max = 0.0_f32;
    for bin in 1..=max_bin {
        let mag = input[bin].norm();
        magnitudes[bin] = mag;
        global_max = global_max.max(mag);
    }

    let mut spectrum = [0.0_f32; SPECTRUM_BANDS];
    if global_max > 0.0 {
        let log_min = SPECTRUM_MIN_FREQ.ln();
        let log_max = SPECTRUM_MAX_FREQ.ln();
        for band_index in 0..SPECTRUM_BANDS {
            let start_ratio = band_index as f32 / SPECTRUM_BANDS as f32;
            let end_ratio = (band_index + 1) as f32 / SPECTRUM_BANDS as f32;
            let start_freq = (log_min + (log_max - log_min) * start_ratio).exp();
            let end_freq = (log_min + (log_max - log_min) * end_ratio).exp();
            let start_bin = ((start_freq / bin_hz).floor() as usize).clamp(1, max_bin);
            let end_bin = ((end_freq / bin_hz).ceil() as usize).clamp(start_bin + 1, max_bin + 1);

            let mut band_energy = 0.0_f32;
            let mut count = 0usize;
            for mag in magnitudes.iter().take(end_bin).skip(start_bin) {
                band_energy += *mag * *mag;
                count += 1;
            }
            if count > 0 {
                let rms = (band_energy / count as f32).sqrt();
                spectrum[band_index] = (rms / global_max).powf(0.72).min(1.0);
            }
        }
    }

    let rms = (rms_acc / sample_count.max(1) as f32).sqrt();
    FrameAnalysis {
        level: (rms * 1.8).powf(0.85).min(1.0),
        spectrum,
    }
}
