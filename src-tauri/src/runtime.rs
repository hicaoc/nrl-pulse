use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::RwLock;

use crate::audio::AudioEngine;
use crate::config::RuntimeConfig;
use crate::models::{
    AtState, ChatMessageEvent, DeviceSettings, PresenceItem, RuntimeBootstrap, SessionSnapshot,
    TimelineEvent,
};
use crate::udp::UdpSession;

fn chrono_local_now() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

fn save_voice_to_wav(
    callsign: &str,
    ssid: u8,
    audio_data: &[i16],
    duration_ms: u128,
    save_path: &str,
) -> Result<String, String> {
    if audio_data.is_empty() {
        return Err("No audio data to save".into());
    }

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
    let file_path = full_dir.join(&filename);

    let sample_rate: u32 = 8000;
    let num_channels: u16 = 1;
    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * u32::from(num_channels) * u32::from(bits_per_sample / 8);
    let block_align = num_channels * (bits_per_sample / 8);
    let data_size = audio_data.len() * 2;

    let mut file = File::create(&file_path)
        .map_err(|e| format!("Failed to create file: {}", e))?;

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

    Ok(filename)
}

#[derive(Clone)]
pub struct RuntimeState {
    inner: Arc<RwLock<RuntimeData>>,
    udp: UdpSession,
    audio: AudioEngine,
    app: Arc<RwLock<Option<AppHandle>>>,
    capture_task_running: Arc<AtomicBool>,
    voice_watchdog_running: Arc<AtomicBool>,
    heartbeat_watchdog_running: Arc<AtomicBool>,
}

struct RuntimeData {
    config: RuntimeConfig,
    snapshot: SessionSnapshot,
    presence: Vec<PresenceItem>,
    timeline: Vec<TimelineEvent>,
    at_state: AtState,
    voice_session: Option<VoiceSession>,
    outgoing_voice_data: Vec<i16>,
    outgoing_voice_start: Option<Instant>,
    last_heartbeat_at: Option<Instant>,
    heartbeat_timeout_reported: bool,
    tick: u64,
}

#[derive(Clone)]
struct VoiceSession {
    callsign: String,
    ssid: u8,
    started_at: Instant,
    last_seen_at: Instant,
    audio_data: Vec<i16>,
}

const SPECTRUM_BANDS: usize = 28;

pub(crate) struct FrameAnalysis {
    pub level: f32,
    pub spectrum: [f32; SPECTRUM_BANDS],
}

impl RuntimeState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(RuntimeData::seed())),
            udp: UdpSession::new(),
            audio: AudioEngine::new(),
            app: Arc::new(RwLock::new(None)),
            capture_task_running: Arc::new(AtomicBool::new(false)),
            voice_watchdog_running: Arc::new(AtomicBool::new(false)),
            heartbeat_watchdog_running: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn set_app_handle(&self, app: AppHandle) {
        let mut guard = self.app.write().await;
        *guard = Some(app);
    }

    pub async fn bootstrap(&self) -> RuntimeBootstrap {
        let guard = self.inner.read().await;
        RuntimeBootstrap {
            snapshot: guard.snapshot.clone(),
            presence: guard.presence.clone(),
            timeline: guard.timeline.clone(),
        }
    }

    pub async fn snapshot(&self) -> SessionSnapshot {
        self.inner.read().await.snapshot.clone()
    }

    pub async fn connect(&self, config: RuntimeConfig) -> SessionSnapshot {
        let maybe_app = self.app.read().await.clone();
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
            guard.snapshot.last_text_message = "正在等待服务器心跳确认".into();
            guard.last_heartbeat_at = None;
            guard.heartbeat_timeout_reported = false;
            guard.presence.clear();
            guard.push_presence(&config.callsign, config.ssid, "本机", "online");
            guard.push_event(
                "开始连接",
                "已发起 NRL UDP 会话，等待服务器心跳确认",
                "accent",
            );
        }
        if let Some(app) = maybe_app {
            if let Err(err) = self.udp.connect(app, self.clone(), config.clone()).await {
                let mut guard = self.inner.write().await;
                guard.snapshot.connection = "disconnected".into();
                guard.snapshot.last_text_message = format!("连接失败: {err}");
                guard.push_event("连接失败", &err, "warn");
                return guard.snapshot.clone();
            }
        }
        self.ensure_audio_started().await;
        self.spawn_capture_forwarder();
        self.spawn_voice_session_watchdog();
        self.spawn_heartbeat_watchdog();
        self.snapshot().await
    }

    pub async fn disconnect(&self) -> SessionSnapshot {
        self.udp.disconnect().await;
        self.audio.stop();
        self.capture_task_running.store(false, Ordering::Relaxed);
        self.heartbeat_watchdog_running
            .store(false, Ordering::Relaxed);
        let mut guard = self.inner.write().await;
        guard.snapshot.connection = "disconnected".into();
        guard.snapshot.is_transmitting = false;
        guard.snapshot.rx_level = 0.0;
        guard.snapshot.tx_level = 0.0;
        guard.snapshot.rx_spectrum.fill(0.0);
        guard.snapshot.tx_spectrum.fill(0.0);
        guard.snapshot.queued_frames = 0;
        guard.snapshot.last_text_message = "会话已断开".into();
        guard.snapshot.active_speaker.clear();
        guard.snapshot.active_speaker_ssid = 0;
        guard.voice_session = None;
        guard.last_heartbeat_at = None;
        guard.heartbeat_timeout_reported = false;
        guard.presence.clear();
        guard.push_event("链路断开", "用户主动断开当前房间连接", "warn");
        guard.snapshot.clone()
    }

    pub async fn toggle_transmit(&self) -> SessionSnapshot {
        let enabled = !self.inner.read().await.snapshot.is_transmitting;
        self.set_transmit(enabled).await
    }

    pub async fn set_transmit(&self, enabled: bool) -> SessionSnapshot {
        let mut guard = self.inner.write().await;
        if guard.snapshot.is_transmitting == enabled {
            return guard.snapshot.clone();
        }
        guard.snapshot.is_transmitting = enabled;
        self.audio.set_transmitting(enabled);
        
        let detail = if enabled {
            guard.outgoing_voice_data.clear();
            guard.outgoing_voice_start = Some(Instant::now());
            "发射链路进入发送状态，等待真实麦克风与编码器挂接"
        } else {
            guard.snapshot.tx_level = 0.0;
            guard.snapshot.tx_spectrum.fill(0.0);
            guard.snapshot.uplink_kbps = 0.0;
            let duration_ms = guard.outgoing_voice_start
                .map(|s| s.elapsed().as_millis())
                .unwrap_or(0);
            let voice_data = guard.outgoing_voice_data.clone();
            guard.outgoing_voice_data.clear();
            guard.outgoing_voice_start = None;
            if !voice_data.is_empty() && duration_ms > 100 {
                let data_copy = voice_data.clone();
                let dur_copy = duration_ms;
                drop(guard);
                self.emit_outgoing_voice_message_with_data(data_copy, dur_copy).await;
                let mut guard = self.inner.write().await;
                guard.push_event("发射切换", "发射结束，返回监听模式", "accent");
                return guard.snapshot.clone();
            }
            "发射结束，返回监听模式"
        };
        guard.push_event("发射切换", detail, "accent");
        guard.snapshot.clone()
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
        guard.snapshot.clone()
    }

    pub async fn update_jitter_buffer(&self, value: u32) -> SessionSnapshot {
        let mut guard = self.inner.write().await;
        guard.snapshot.devices.jitter_buffer_ms = value;
        guard.snapshot.jitter_ms = value.saturating_sub(34);
        guard.push_event(
            "调整缓冲",
            &format!("抖动缓冲更新为 {value}ms，后续会驱动真实 jitter buffer"),
            "accent",
        );
        guard.snapshot.clone()
    }

    pub async fn send_text_message(
        &self,
        config: &RuntimeConfig,
        message: String,
    ) -> SessionSnapshot {
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
        guard.snapshot.clone()
    }

    pub async fn udp_send_at_state(
        &self,
        config: &RuntimeConfig,
        lines: &[String],
    ) -> Result<(), String> {
        self.udp.send_at_state(config, lines).await
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
        let now = Instant::now();
        let emitted = Vec::new();
        {
            let mut guard = self.inner.write().await;
            guard.snapshot.active_speaker = callsign.clone();
            guard.snapshot.active_speaker_ssid = ssid;
            guard.snapshot.rx_level = level;
            guard.snapshot.rx_spectrum = spectrum.to_vec();
            guard.snapshot.queued_frames = (samples / 160).max(1) as u32;
            guard.snapshot.downlink_kbps = packet_kbps(samples / 2);
            guard.push_presence(&callsign, ssid, "远端节点", "speaking");

            match guard.voice_session.take() {
                Some(mut session) if session.callsign == callsign && session.ssid == ssid => {
                    session.last_seen_at = now;
                    session.audio_data.extend_from_slice(pcm_data);
                    guard.voice_session = Some(session);
                }
                Some(session) => {
                    let elapsed = now.duration_since(session.started_at).as_millis();
                    self.emit_voice_message(&session, elapsed).await;
                    let mut new_session = VoiceSession {
                        callsign: callsign.clone(),
                        ssid,
                        started_at: now,
                        last_seen_at: now,
                        audio_data: Vec::new(),
                    };
                    new_session.audio_data.extend_from_slice(pcm_data);
                    guard.voice_session = Some(new_session);
                }
                None => {
                    let mut new_session = VoiceSession {
                        callsign: callsign.clone(),
                        ssid,
                        started_at: now,
                        last_seen_at: now,
                        audio_data: Vec::new(),
                    };
                    new_session.audio_data.extend_from_slice(pcm_data);
                    guard.voice_session = Some(new_session);
                }
            }
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
            let _ = app.emit("runtime://snapshot", self.snapshot().await);
        }
    }

    pub fn enqueue_received_pcm(&self, pcm: &[i16]) {
        self.audio.enqueue_received_pcm(pcm);
    }

    pub async fn note_remote_activity(&self, callsign: &str, ssid: u8, state: &str) {
        let mut guard = self.inner.write().await;
        guard.push_presence(callsign, ssid, "远端节点", state);
    }

    pub async fn note_heartbeat(&self, callsign: &str, ssid: u8) {
        let mut emitted = Vec::new();
        {
            let mut guard = self.inner.write().await;
            let first_confirm = guard.last_heartbeat_at.is_none();
            let recovered = guard.heartbeat_timeout_reported;
            guard.push_presence(callsign, ssid, "远端节点", "online");
            guard.snapshot.connection = "connected".into();
            guard.snapshot.last_text_message = format!("收到 {}-{} 心跳确认", callsign, ssid);
            guard.last_heartbeat_at = Some(Instant::now());
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
                eprintln!("[Runtime] audio.start() OK: input={} output={}", devices.input_device, devices.output_device);
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
                        devices.input_device, input_mode,
                        devices.output_device, output_mode
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
                let analysis = analyze_pcm_frame(&frame);
                runtime
                    .note_transmit_frame(frame.len(), analysis.level, &analysis.spectrum)
                    .await;
                let is_transmitting = runtime.inner.read().await.snapshot.is_transmitting;
                if is_transmitting {
                    let mut guard = runtime.inner.write().await;
                    guard.outgoing_voice_data.extend_from_slice(&frame);
                }
                let _ = runtime.udp.send_voice_frame(&config, &frame).await;
            }
            runtime.capture_task_running.store(false, Ordering::Relaxed);
        });
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
                    let Some(session) = guard.voice_session.take() else {
                        continue;
                    };
                    if session.last_seen_at.elapsed() < Duration::from_secs(1) {
                        guard.voice_session = Some(session);
                        continue;
                    }
                    let elapsed = session
                        .last_seen_at
                        .duration_since(session.started_at)
                        .as_millis();
                    guard.snapshot.rx_level = 0.0;
                    guard.snapshot.rx_spectrum.fill(0.0);
                    guard.snapshot.queued_frames = 0;
                    guard.snapshot.downlink_kbps = 0.0;
                    guard.snapshot.active_speaker.clear();
                    guard.snapshot.active_speaker_ssid = 0;
                    Some((session, elapsed))
                };
                if let Some((session, elapsed)) = session_info {
                    runtime.emit_voice_message(&session, elapsed).await;
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
                audio_data: None,
                duration: None,
            };
            let _ = app.emit("runtime://chat-message", event);
        }
    }

    async fn emit_voice_message(&self, session: &VoiceSession, duration_ms: u128) {
        let config = {
            let guard = self.inner.read().await;
            guard.config.clone()
        };

        if let Err(e) = save_voice_to_wav(
            &session.callsign,
            session.ssid,
            &session.audio_data,
            duration_ms,
            &config.voice_save_path,
        ) {
            eprintln!("[Runtime] Failed to save voice: {}", e);
        }

        if let Some(app) = self.app.read().await.clone() {
            let id = {
                let mut guard = self.inner.write().await;
                guard.tick += 1;
                guard.tick
            };
            let time = chrono_local_now();
            let audio_data: Vec<f32> = session
                .audio_data
                .iter()
                .map(|&s| s as f32 / i16::MAX as f32)
                .collect();
            let duration_sec = duration_ms as f64 / 1000.0;
            let event = ChatMessageEvent {
                id: format!("chat-{id}"),
                side: "remote".into(),
                text: String::new(),
                meta: format!("{}-{}-{}", session.callsign, session.ssid, format!("{:.1}s", duration_sec)),
                time,
                type_: Some("voice".into()),
                audio_data: Some(audio_data),
                duration: Some(duration_sec),
            };
            let _ = app.emit("runtime://chat-message", event);
        }
    }

    async fn emit_outgoing_voice_message_with_data(&self, audio_data: Vec<i16>, duration_ms: u128) {
        let (callsign, ssid, voice_save_path) = {
            let guard = self.inner.read().await;
            let config = &guard.config;
            (config.callsign.clone(), config.ssid, config.voice_save_path.clone())
        };
        
        if audio_data.is_empty() || duration_ms < 100 {
            return;
        }
        
        if let Err(e) = save_voice_to_wav(
            &callsign,
            ssid,
            &audio_data,
            duration_ms,
            &voice_save_path,
        ) {
            eprintln!("[Runtime] Failed to save outgoing voice: {}", e);
        }
        
        if let Some(app) = self.app.read().await.clone() {
            let id = {
                let mut guard = self.inner.write().await;
                guard.tick += 1;
                guard.tick
            };
            let time = chrono_local_now();
            let normalized_data: Vec<f32> = audio_data
                .iter()
                .map(|&s| s as f32 / i16::MAX as f32)
                .collect();
            let duration_sec = duration_ms as f64 / 1000.0;
            let event = ChatMessageEvent {
                id: format!("chat-{id}"),
                side: "self".into(),
                text: String::new(),
                meta: format!("{}-{}-{}", callsign, ssid, format!("{:.1}s", duration_sec)),
                time,
                type_: Some("voice".into()),
                audio_data: Some(normalized_data),
                duration: Some(duration_sec),
            };
            let _ = app.emit("runtime://chat-message", event);
        }
    }
}

impl RuntimeData {
    fn seed() -> Self {
        Self {
            config: RuntimeConfig::default(),
            snapshot: SessionSnapshot {
                room_name: "NRL East Hub".into(),
                callsign: "B1NRL".into(),
                ssid: 110,
                active_speaker: String::new(),
                active_speaker_ssid: 0,
                connection: "disconnected".into(),
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
            voice_session: None,
            outgoing_voice_data: vec![],
            outgoing_voice_start: None,
            last_heartbeat_at: None,
            heartbeat_timeout_reported: false,
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
    let mut spectrum = [0.0_f32; SPECTRUM_BANDS];
    let chunk_size = ((samples.len() + SPECTRUM_BANDS - 1) / SPECTRUM_BANDS).max(1);

    for (band_index, chunk) in samples.chunks(chunk_size).take(SPECTRUM_BANDS).enumerate() {
        let mut band_peak = 0.0_f32;
        for &sample in chunk {
            let normalized = (sample as f32 / i16::MAX as f32).abs();
            peak = peak.max(normalized);
            band_peak = band_peak.max(normalized);
        }
        spectrum[band_index] = band_peak.powf(0.72).min(1.0);
    }

    FrameAnalysis {
        level: peak.powf(0.78).min(1.0),
        spectrum,
    }
}
