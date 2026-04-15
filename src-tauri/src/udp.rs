use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tauri::AppHandle;
use tokio::net::{lookup_host, UdpSocket};
use tokio::sync::{Mutex, RwLock};

use crate::at::{decode_at, encode_at};
use crate::config::RuntimeConfig;
use crate::g711::{decode_alaw_frame, encode_alaw_frame};
use crate::nrl::NrlPacket;
use crate::runtime::RuntimeState;

#[derive(Clone)]
pub struct UdpSession {
    socket: Arc<Mutex<Option<Arc<UdpSocket>>>>,
    remote_addr: Arc<RwLock<Option<SocketAddr>>>,
    heartbeat_running: Arc<AtomicBool>,
    session_id: Arc<AtomicU64>,
}

impl UdpSession {
    pub fn new() -> Self {
        Self {
            socket: Arc::new(Mutex::new(None)),
            remote_addr: Arc::new(RwLock::new(None)),
            heartbeat_running: Arc::new(AtomicBool::new(false)),
            session_id: Arc::new(AtomicU64::new(0)),
        }
    }

    pub async fn connect(
        &self,
        app: AppHandle,
        runtime: RuntimeState,
        config: RuntimeConfig,
    ) -> Result<(), String> {
        self.disconnect().await;
        let target = format!("{}:{}", config.server, config.port);
        let remote: SocketAddr = lookup_host(&target)
            .await
            .map_err(|err| format!("resolve udp address failed: {err}"))?
            .next()
            .ok_or_else(|| format!("no udp address resolved for {target}"))?;
        let socket = Arc::new(
            UdpSocket::bind("0.0.0.0:0")
                .await
                .map_err(|err| format!("bind udp failed: {err}"))?,
        );
        socket
            .connect(remote)
            .await
            .map_err(|err| format!("connect udp failed: {err}"))?;

        {
            let mut guard = self.socket.lock().await;
            *guard = Some(socket.clone());
        }
        {
            let mut guard = self.remote_addr.write().await;
            *guard = Some(remote);
        }

        self.heartbeat_running.store(true, Ordering::Relaxed);
        let session_id = self.session_id.fetch_add(1, Ordering::Relaxed) + 1;

        self.spawn_heartbeat(socket.clone(), runtime.clone(), config.clone(), session_id);
        self.spawn_reader(app, socket, runtime, config, session_id);
        Ok(())
    }

    pub async fn disconnect(&self) {
        self.heartbeat_running.store(false, Ordering::Relaxed);
        self.session_id.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.socket.lock().await;
        *guard = None;
        let mut remote = self.remote_addr.write().await;
        *remote = None;
    }

    pub async fn send_text(&self, config: &RuntimeConfig, message: &str) -> Result<(), String> {
        let payload = message.as_bytes().to_vec();
        self.send_packet(NrlPacket::text_message(
            &config.callsign,
            config.ssid,
            payload,
        ))
        .await
    }

    pub async fn send_at_state(
        &self,
        config: &RuntimeConfig,
        lines: &[String],
    ) -> Result<(), String> {
        self.send_packet(NrlPacket::at_message(
            &config.callsign,
            config.ssid,
            encode_at(lines),
        ))
        .await
    }

    pub async fn send_voice_frame(
        &self,
        config: &RuntimeConfig,
        pcm: &[i16],
    ) -> Result<(), String> {
        let encoded = encode_alaw_frame(pcm, config.volume);
        self.send_packet(NrlPacket::voice_frame(
            &config.callsign,
            config.ssid,
            encoded,
        ))
        .await
    }

    async fn send_packet(&self, packet: NrlPacket) -> Result<(), String> {
        let socket = {
            let guard = self.socket.lock().await;
            guard.clone()
        };
        let Some(socket) = socket else {
            return Err("udp session is not connected".into());
        };

        socket
            .send(&packet.encode())
            .await
            .map_err(|err| format!("udp send failed: {err}"))?;
        Ok(())
    }

    fn spawn_heartbeat(
        &self,
        socket: Arc<UdpSocket>,
        runtime: RuntimeState,
        config: RuntimeConfig,
        session_id: u64,
    ) {
        let heartbeat_running = self.heartbeat_running.clone();
        let active_session_id = self.session_id.clone();
        tauri::async_runtime::spawn(async move {
            let packet = NrlPacket::heartbeat(&config.callsign, config.ssid);
            if socket.send(&packet.encode()).await.is_err() {
                runtime
                    .push_runtime_event("心跳失败", "首次 UDP 心跳发送失败", "warn")
                    .await;
                return;
            }
            runtime.note_heartbeat_sent().await;
            runtime
                .push_runtime_event("心跳已发送", "已发出首次 UDP 心跳", "info")
                .await;
            loop {
                if !heartbeat_running.load(Ordering::Relaxed)
                    || active_session_id.load(Ordering::Relaxed) != session_id
                {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
                if !heartbeat_running.load(Ordering::Relaxed)
                    || active_session_id.load(Ordering::Relaxed) != session_id
                {
                    break;
                }
                let packet = NrlPacket::heartbeat(&config.callsign, config.ssid);
                if socket.send(&packet.encode()).await.is_err() {
                    runtime
                        .push_runtime_event("心跳失败", "UDP 心跳发送失败，等待上层恢复", "warn")
                        .await;
                    break;
                }
                runtime.note_heartbeat_sent().await;
            }
        });
    }

    fn spawn_reader(
        &self,
        app: AppHandle,
        socket: Arc<UdpSocket>,
        runtime: RuntimeState,
        config: RuntimeConfig,
        session_id: u64,
    ) {
        let active_session_id = self.session_id.clone();
        tauri::async_runtime::spawn(async move {
            let mut buf = [0_u8; 2048];
            loop {
                if active_session_id.load(Ordering::Relaxed) != session_id {
                    break;
                }
                let size = match tokio::time::timeout(Duration::from_secs(1), socket.recv(&mut buf))
                    .await
                {
                    Ok(Ok(size)) => size,
                    Ok(Err(err)) => {
                        runtime
                            .push_runtime_event("接收中断", &format!("UDP 接收异常: {err}"), "warn")
                            .await;
                        break;
                    }
                    Err(_) => {
                        continue;
                    }
                };
                if active_session_id.load(Ordering::Relaxed) != session_id {
                    break;
                }

                match NrlPacket::decode(&buf[..size]) {
                    Ok(packet) => {
                        handle_packet(&app, &runtime, &config, packet).await;
                    }
                    Err(err) => {
                        runtime
                            .push_runtime_event("报文解析失败", &err, "warn")
                            .await;
                    }
                }
            }
        });
    }
}

async fn handle_packet(
    app: &AppHandle,
    runtime: &RuntimeState,
    _config: &RuntimeConfig,
    packet: NrlPacket,
) {
    match packet.packet_type {
        1 => {
            let pcm = decode_alaw_frame(&packet.data);
            let analysis = crate::runtime::analyze_pcm_frame(&pcm);
            runtime.enqueue_received_pcm(&pcm);
            runtime
                .note_voice_frame(
                    packet.callsign_string(),
                    packet.ssid,
                    pcm.len(),
                    analysis.level,
                    &analysis.spectrum,
                    &pcm,
                )
                .await;
            runtime.throttled_emit_audio_state(app).await;
            // 限速 emit：每 80ms 最多推一次 snapshot，防止高频 UDP 包导致前端事件积压
            runtime.throttled_emit_snapshot(app).await;
        }
        2 => {
            runtime
                .note_heartbeat(&packet.callsign_string(), packet.ssid)
                .await;
        }
        5 => {
            let text = String::from_utf8_lossy(&packet.data).to_string();
            runtime
                .note_remote_activity(&packet.callsign_string(), packet.ssid, "online")
                .await;
            runtime.apply_text_message(&text).await;
            runtime
                .note_text_message(&text, &packet.callsign_string(), packet.ssid)
                .await;
            runtime.push_runtime_event("收到文本", &text, "info").await;
        }
        11 => {
            if let Some(at) = decode_at(&packet.data) {
                runtime
                    .note_remote_activity(&packet.callsign_string(), packet.ssid, "online")
                    .await;
                runtime.apply_at_command(&at.command, &at.value).await;
                runtime
                    .push_runtime_event(
                        "收到 AT 指令",
                        &format!("{}={}", at.command, at.value),
                        "accent",
                    )
                    .await;
            } else {
                runtime
                    .push_runtime_event("AT 解析失败", "收到无法识别的 AT 数据", "warn")
                    .await;
            }
        }
        _ => {
            runtime
                .push_runtime_event(
                    "收到其他报文",
                    &format!("packet type={}", packet.packet_type),
                    "info",
                )
                .await;
        }
    }
}
