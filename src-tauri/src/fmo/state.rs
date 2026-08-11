//! FMO 运行时状态：证书库 / APRS / MQTT / 收发音频 / 收藏的统一编排。
//!
//! 事件通道：所有内部 emit 通过 `fmo://event` 发送 JSON（含 `type` 字段），
//! 与 sim-rust WS 协议保持一致，前端单一 listener 分发。

use crate::fmo::aprs::{AprsClient, AprsParams, EmitFn, ServerTable};
use crate::fmo::audio::{RxAudio, TxSession};
use crate::fmo::certstore::CertStore;
use crate::fmo::fmo_auth;
use crate::fmo::fmo_frame;
use crate::fmo::mqtt_client::FmoMqttClient;
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex;

pub const DEFAULT_APRS_HOST: &str = "rotate.aprs2.net";
pub const DEFAULT_APRS_PORT: u16 = 10152;

pub struct FmoState {
    pub data_dir: PathBuf,
    app: Arc<std::sync::Mutex<Option<AppHandle>>>,
    pub emit: EmitFn,
    pub server_table: Arc<ServerTable>,
    pub aprs_client: Arc<AprsClient>,
    pub cert_store: Arc<CertStore>,
    pub mqtt_client: Arc<FmoMqttClient>,
    pub rx_audio: Arc<RxAudio>,
    pub tx_session: Arc<Mutex<Option<Arc<TxSession>>>>,
    pub rx_play_enabled: Arc<std::sync::Mutex<bool>>,
    pub rx_loop_enabled: Arc<std::sync::Mutex<bool>>,
    pub selected_server: Arc<Mutex<serde_json::Value>>,
    pub favorites: Arc<Mutex<Vec<serde_json::Value>>>,
    pub favorites_path: PathBuf,
    /// 当前发言呼号（raw handler 在解码前更新，on_pcm 读取用于 note_voice_frame）。
    pub current_speaker: Arc<std::sync::Mutex<String>>,
    /// 事件桥接：每条 emit 同时转交 runtime 更新 snapshot/timeline。
    pub bridge: Arc<std::sync::Mutex<Option<Arc<dyn Fn(serde_json::Value) + Send + Sync>>>>,
    /// FMO 独立统计：接收语音帧数 / 发射帧数 / 累计文本等
    pub stats: FmoStats,
    /// 手动配置的 FMO 呼号（优先于证书），空则用证书呼号
    pub configured_callsign: Arc<std::sync::Mutex<String>>,
    aprs_task_running: Arc<AtomicBool>,
    identity_watch_running: Arc<AtomicBool>,
}

/// FMO 独立统计计数（与 NRL 分离）。
#[derive(Clone, Default)]
pub struct FmoStats {
    pub rx_frames: Arc<std::sync::atomic::AtomicU64>,
    pub tx_frames: Arc<std::sync::atomic::AtomicU64>,
    pub rx_text: Arc<std::sync::atomic::AtomicU64>,
    pub tx_packets: Arc<std::sync::atomic::AtomicU64>,
    pub server_info: Arc<std::sync::atomic::AtomicU64>,
    /// FMO 独立实时显示：接收/发射电平与 28 频段频谱（与 NRL 分离）
    pub rx_level: Arc<std::sync::Mutex<f32>>,
    pub rx_spectrum: Arc<std::sync::Mutex<Vec<f32>>>,
    pub tx_level: Arc<std::sync::Mutex<f32>>,
    pub tx_spectrum: Arc<std::sync::Mutex<Vec<f32>>>,
    /// FMO 独立网络质量指标（与 NRL 分离）
    pub jitter_ms: Arc<std::sync::Mutex<u32>>,
    pub latency_ms: Arc<std::sync::Mutex<u32>>,
    pub packet_loss: Arc<std::sync::Mutex<f32>>,
    pub queued_frames: Arc<std::sync::Mutex<u32>>,
    pub downlink_kbps: Arc<std::sync::Mutex<f32>>,
    pub uplink_kbps: Arc<std::sync::Mutex<f32>>,
    /// 当前接收语音编码（"ADPCM" | "OPUS"），供呼号区右下角显示
    pub rx_codec: Arc<std::sync::Mutex<String>>,
}

impl FmoStats {
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "rxFrames": self.rx_frames.load(std::sync::atomic::Ordering::Relaxed),
            "txFrames": self.tx_frames.load(std::sync::atomic::Ordering::Relaxed),
            "rxText": self.rx_text.load(std::sync::atomic::Ordering::Relaxed),
            "txPackets": self.tx_packets.load(std::sync::atomic::Ordering::Relaxed),
            "serverInfo": self.server_info.load(std::sync::atomic::Ordering::Relaxed),
            "rxLevel": *self.rx_level.lock().unwrap(),
            "rxSpectrum": self.rx_spectrum.lock().unwrap().clone(),
            "txLevel": *self.tx_level.lock().unwrap(),
            "txSpectrum": self.tx_spectrum.lock().unwrap().clone(),
            "jitterMs": *self.jitter_ms.lock().unwrap(),
            "latencyMs": *self.latency_ms.lock().unwrap(),
            "packetLoss": *self.packet_loss.lock().unwrap(),
            "queuedFrames": *self.queued_frames.lock().unwrap(),
            "downlinkKbps": *self.downlink_kbps.lock().unwrap(),
            "uplinkKbps": *self.uplink_kbps.lock().unwrap(),
        })
    }
}

impl FmoState {
    pub fn new(data_dir: PathBuf) -> Self {
        std::fs::create_dir_all(&data_dir).ok();
        let app: Arc<std::sync::Mutex<Option<AppHandle>>> = Arc::new(std::sync::Mutex::new(None));
        let emit_app = app.clone();
        let bridge = Arc::new(std::sync::Mutex::new(None::<Arc<dyn Fn(serde_json::Value) + Send + Sync>>));
        let emit_bridge = bridge.clone();
        let emit: EmitFn = Arc::new(move |ev| {
            if let Some(app) = emit_app.lock().unwrap().as_ref() {
                let _ = app.emit("fmo://event", ev.clone());
            }
            if let Some(b) = emit_bridge.lock().unwrap().as_ref() {
                b(ev);
            }
        });

        let server_table = Arc::new(ServerTable::new(Some(data_dir.join("servers.json"))));
        let mqtt_client = Arc::new(FmoMqttClient::new(emit.clone()));
        let cert_store = Arc::new(CertStore::new(data_dir.join("certs")));
        let aprs_client = Arc::new(AprsClient::new(emit.clone(), server_table.clone()));
        let rx_audio = Arc::new(RxAudio::new().unwrap_or_else(|_| panic!("opus 初始化失败")));

        let favorites_path = data_dir.join("favorites.json");
        let favorites: Vec<serde_json::Value> = std::fs::read_to_string(&favorites_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();

        // 上次选定的服务器（含证书指纹），启动时恢复用于自动连接
        let selected: serde_json::Value =
            std::fs::read_to_string(data_dir.join("selected_server.json"))
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or(serde_json::Value::Null);

        Self {
            data_dir,
            app,
            emit,
            server_table,
            aprs_client,
            cert_store,
            mqtt_client,
            rx_audio,
            tx_session: Arc::new(Mutex::new(None)),
            rx_play_enabled: Arc::new(std::sync::Mutex::new(true)),
            rx_loop_enabled: Arc::new(std::sync::Mutex::new(false)),
            selected_server: Arc::new(Mutex::new(selected)),
            favorites: Arc::new(Mutex::new(favorites)),
            favorites_path,
            current_speaker: Arc::new(std::sync::Mutex::new(String::new())),
            bridge,
            stats: FmoStats::default(),
            configured_callsign: Arc::new(std::sync::Mutex::new(String::new())),
            aprs_task_running: Arc::new(AtomicBool::new(false)),
            identity_watch_running: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn set_app(&self, app: AppHandle) {
        *self.app.lock().unwrap() = Some(app);
    }

    pub fn current_callsign(&self) -> String {
        let configured = self.configured_callsign.lock().unwrap().clone();
        if !configured.trim().is_empty() {
            return configured.trim().to_string();
        }
        let p = self.data_dir.join("certs").join("cert_user.json");
        if let Ok(text) = std::fs::read_to_string(&p) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(cs) = v["subject"]["callsign"].as_str() {
                    return cs.to_string();
                }
            }
        }
        "N0CALL".into()
    }

    pub fn current_uid(&self) -> u32 {
        let p = self.data_dir.join("certs").join("cert_user.json");
        if let Ok(text) = std::fs::read_to_string(&p) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(uid) = v["subject"]["uid"].as_u64() {
                    return uid as u32;
                }
            }
        }
        0
    }

    /// 选定服务器的认证信息（host/uid/证书指纹），供构建 SAS 凭据。
    async fn server_auth_json(&self) -> Result<serde_json::Value, String> {
        let mut srv = {
            let sel = self.selected_server.lock().await;
            match sel.as_object() {
                Some(m) => serde_json::Value::Object(m.clone()),
                None => serde_json::Value::Null,
            }
        };
        if srv.is_null() {
            return Err("未选定服务器（先在服务器页点选定）".into());
        }
        let cert_info = srv.get("cert").cloned().unwrap_or(serde_json::Value::Null);
        if srv.get("uid").is_none() && cert_info.is_object() {
            srv["uid"] = cert_info.get("uid").cloned().unwrap_or(serde_json::Value::Null);
        }
        if cert_info.is_object() {
            srv["fingerprint"] = serde_json::Value::Array(
                fmo_auth::beacon_cert_fingerprint(&cert_info)
                    .into_iter()
                    .map(serde_json::Value::from)
                    .collect(),
            );
            if let Some(cs) = cert_info.get("callsign") {
                srv["callsign"] = cs.clone();
            }
        }
        let ok_host = srv.get("host").and_then(|h| h.as_str()).map(|h| !h.is_empty()).unwrap_or(false);
        let ok_uid = srv.get("uid").is_some();
        let ok_fp = srv.get("fingerprint").is_some();
        if !(ok_host && ok_uid && ok_fp) {
            return Err("选定服务器缺少 uid/证书指纹（请从 STATION 广播列表选择）".into());
        }
        Ok(srv)
    }

    /// 返回 {username, password, role} 的 MQTT 凭据（基于选定服务器 + 证书库）。
    /// 初始角色：服务器呼号与证书呼号一致（自己的服务器）用 super，否则 user。
    pub async fn mqtt_credentials(&self) -> Result<serde_json::Value, String> {
        let srv = self.server_auth_json().await?;
        let certs_dir = self.data_dir.join("certs");
        fmo_auth::validate_identity(&certs_dir)?;
        let role = fmo_auth::initial_role(&certs_dir, &srv);
        fmo_auth::mqtt_credentials(&certs_dir, &srv, &role)
    }

    /// 连接 MQTT（使用选定服务器 + 证书自动构建凭据）。
    pub async fn connect_mqtt(&self, tls: bool) -> Result<(), String> {
        let creds = match self.mqtt_credentials().await {
            Ok(c) => c,
            Err(e) => {
                (self.emit)(json!({
                    "type": "mqtt_state", "state": "error", "detail": format!("凭据构建失败: {e}")
                }));
                (self.emit)(json!({
                    "type": "log", "level": "error", "msg": format!("FMO MQTT 凭据构建失败：{e}")
                }));
                return Err(e);
            }
        };
        // host/port 从选定服务器取；creds 只含 username/password
        let sel = self.selected_server.lock().await.clone();
        let host = sel.get("host").and_then(|h| h.as_str()).unwrap_or("").to_string();
        let port = sel.get("port").and_then(|p| p.as_u64()).unwrap_or(1883) as u16;
        if host.is_empty() {
            let msg: String = "选定服务器缺少 host 地址，请从服务器列表重新选择".into();
            (self.emit)(json!({"type": "mqtt_state", "state": "error", "detail": msg.clone()}));
            (self.emit)(json!({"type": "log", "level": "error", "msg": msg.clone()}));
            return Err(msg);
        }
        // 安装凭据工厂：认证被拒时 MQTT 客户端从初始角色起按 ROLE_SEQ 往后换角色重试
        if let Ok(srv) = self.server_auth_json().await {
            // 诊断日志：SAS 凭据的实际目标与用户身份，便于排查「别的服务器能登、自己服务器被拒」
            let fp_hex = srv["fingerprint"].as_array()
                .map(|a| a.iter().take(8)
                    .filter_map(|b| b.as_u64())
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>())
                .unwrap_or_default();
            (self.emit)(json!({"type": "log", "level": "info",
                "msg": format!("SAS 凭据：用户 {} uid={} 角色={} → 服务器 {} uid={} {}:{} 证书fp={}…",
                               creds["username"].as_str().unwrap_or("-"),
                               self.current_uid(),
                               creds["role"].as_str().unwrap_or("user"),
                               srv["callsign"].as_str().unwrap_or("-"),
                               srv["uid"].as_u64().unwrap_or(0),
                               host, port, fp_hex)}));
            let certs_dir = self.data_dir.join("certs");
            *self.mqtt_client.cred_factory.lock().unwrap() =
                Some(std::sync::Arc::new(move |role: &str| {
                    let c = fmo_auth::mqtt_credentials(&certs_dir, &srv, role)?;
                    let u = c["username"].as_str().unwrap_or("").to_string();
                    let p = c["password"].as_str().unwrap_or("").to_string();
                    Ok((u, p))
                }));
        }
        let username = creds.get("username").and_then(|u| u.as_str()).map(|s| s.to_string());
        let password = creds.get("password").and_then(|p| p.as_str()).map(|s| s.to_string());
        let role = creds.get("role").and_then(|r| r.as_str()).unwrap_or("user").to_string();
        let callsign = username.clone();
        let uid = self.current_uid();
        eprintln!(
            "[FMO] connect_mqtt host={host} port={port} uid={uid} user={} role={role} pw_len={}",
            username.as_deref().unwrap_or("-"),
            password.as_deref().map(|p| p.len()).unwrap_or(0)
        );
        self.mqtt_client
            .connect(&host, port, uid, username, password, tls, callsign, role)
            .await;
        Ok(())
    }

    pub async fn disconnect_mqtt(&self) {
        self.mqtt_client.disconnect().await;
    }

    /// 启动 APRS-IS 后台任务（常驻，内部自动重连）。
    pub fn ensure_aprs_task(&self) {
        if self.aprs_task_running.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let client = self.aprs_client.clone();
        tauri::async_runtime::spawn(async move {
            client.run().await;
        });
    }

    /// 证书身份巡检：启动即查一次，之后每 10 分钟复查。
    /// 证书后来过期、私钥与证书不配套、临近 7 天到期都会以 warn 日志提醒
    /// （状态不变不重复提醒，修复后再次出现问题会重新提醒）。
    pub fn start_identity_watchdog(self: &Arc<Self>) {
        if self.identity_watch_running.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut last_msg = String::new();
            loop {
                let certs_dir = this.data_dir.join("certs");
                let msg = match fmo_auth::identity_status(&certs_dir) {
                    Some(Err(e)) => e,
                    _ => String::new(),
                };
                if !msg.is_empty() && msg != last_msg {
                    (this.emit)(json!({"type": "log", "level": "warn",
                        "msg": format!("FMO 证书提醒：{msg}")}));
                }
                last_msg = msg;
                tokio::time::sleep(std::time::Duration::from_secs(600)).await;
            }
        });
    }

    /// 自动连接 APRS（证书存在时）。
    pub async fn auto_connect_aprs(&self) {
        let cert_path = self.data_dir.join("certs").join("cert_user.json");
        if !cert_path.is_file() {
            return;
        }
        if let Ok(text) = std::fs::read_to_string(&cert_path) {
            if let Ok(cert) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(cs) = cert["subject"]["callsign"].as_str() {
                    if !cs.is_empty() {
                        let params = AprsParams {
                            host: DEFAULT_APRS_HOST.to_string(),
                            port: DEFAULT_APRS_PORT,
                            callsign: cs.to_string(),
                            passcode: fmo_auth::aprs_passcode(cs),
                            lat: 39.9,
                            lon: 116.4,
                            dist: 500.0,
                        };
                        self.aprs_client.connect_to(params).await;
                        (self.emit)(json!({"type": "log", "level": "info",
                            "msg": format!("已检测到证书，自动连接 APRS-IS（{}）", cs)}));
                    }
                }
            }
        }
    }

    /// 启动时自动连接 MQTT 的条件：证书就位 + 已选定服务器 + 当前未连接。
    pub async fn mqtt_autoconnect_ready(&self) -> bool {
        self.data_dir.join("certs").join("cert_user.json").is_file()
            && !self.selected_server.lock().await.is_null()
            && self.mqtt_client.state_str().await == "disconnected"
    }

    /// 安装 FMO/RAW 解码回调：帧 → 拆 Opus/ADPCM → on_pcm。
    pub fn install_raw_handler(&self) {
        let rx_audio = self.rx_audio.clone();
        let rx_play = self.rx_play_enabled.clone();
        let rx_loop = self.rx_loop_enabled.clone();
        let emit = self.emit.clone();
        let speaker = self.current_speaker.clone();
        let callsign = self.current_callsign();
        let stats = self.stats.clone();
        self.mqtt_client.on_raw_payload.lock().unwrap().replace(
            Arc::new(move |payload: Vec<u8>| {
                let Some(p) = fmo_frame::parse_frame(&payload) else { return };
                if p.packets.is_empty() && p.adpcm.is_empty() {
                    return;
                }
                stats
                    .rx_frames
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                *speaker.lock().unwrap() = p.callsign.clone();
                let mine = p.callsign == callsign;
                if mine {
                    emit(json!({"type": "log", "level": "info",
                        "msg": format!("回环确认：本机 {} 包已送达服务器并返回", p.packets.len())}));
                    if !*rx_loop.lock().unwrap() {
                        return;
                    }
                }
                if !*rx_play.lock().unwrap() {
                    return;
                }
                if !p.adpcm.is_empty() {
                    *stats.rx_codec.lock().unwrap() = "ADPCM".to_string();
                    rx_audio.feed_adpcm(&p.adpcm);
                } else if !p.packets.is_empty() {
                    *stats.rx_codec.lock().unwrap() = "OPUS".to_string();
                    let _ = rx_audio.feed_packets(&p.packets);
                }
            }),
        );
    }

    /// 选定服务器并持久化（下次启动恢复，用于自动连接）。
    pub async fn select_server(&self, server: serde_json::Value) {
        {
            let mut sel = self.selected_server.lock().await;
            *sel = server.clone();
        }
        if let Ok(text) = serde_json::to_string_pretty(&server) {
            std::fs::write(self.data_dir.join("selected_server.json"), text).ok();
        }
    }

    /// 默认选定一台带证书信息的在线服务器（已有选定项时不覆盖）。
    pub async fn select_default_server(&self) {
        if !self.selected_server.lock().await.is_null() {
            return;
        }
        let list = self.server_table.to_list().await;
        let mut cands: Vec<serde_json::Value> = list.into_iter()
            .filter(|s| {
                s.get("host").and_then(|h| h.as_str()).map(|h| !h.is_empty()).unwrap_or(false)
                    && s.get("cert").and_then(|c| c.get("uid")).is_some()
            })
            .collect();
        cands.sort_by_key(|s| std::cmp::Reverse(s.get("online").and_then(|o| o.as_i64()).unwrap_or(0)));
        if let Some(s) = cands.into_iter().next() {
            let mut sel = self.selected_server.lock().await;
            *sel = json!({
                "host": s.get("host"),
                "port": s.get("port").cloned().unwrap_or(json!(1883)),
                "callsign": s.get("cert").and_then(|c| c.get("callsign")).cloned().unwrap_or(json!("")),
                "uid": s.get("cert").and_then(|c| c.get("uid")),
                "cert": s.get("cert"),
            });
        }
    }

    /// PTT 开始。
    pub async fn start_tx(&self, mode: &str) -> Result<(), String> {
        if self.mqtt_client.state_str().await != "connected" {
            return Err("MQTT 未连接，不能发射".into());
        }
        let callsign = self.current_callsign();
        let ts = Arc::new(TxSession::new(
            self.mqtt_client.clone(),
            &callsign,
            mode,
            Some(self.stats.tx_frames.clone()),
        )?);
        *self.tx_session.lock().await = Some(ts);
        Ok(())
    }

    /// PTT 喂 PCM（8k s16le）。
    pub async fn feed_pcm(&self, pcm: &[i16]) {
        let tx = self.tx_session.lock().await.clone();
        if let Some(ts) = tx {
            ts.feed_pcm(pcm).await;
        }
    }

    /// PTT 结束。
    pub async fn stop_tx(&self) {
        let tx = self.tx_session.lock().await.clone();
        if let Some(ts) = tx {
            ts.stop().await;
        }
        *self.tx_session.lock().await = None;
    }

    // ---------------------------------------------------------------- 收藏

    pub async fn favorites_list(&self) -> Vec<serde_json::Value> {
        self.favorites.lock().await.clone()
    }

    pub async fn favorites_add(&self, body: serde_json::Value) -> serde_json::Value {
        let host = body.get("host").and_then(|h| h.as_str()).unwrap_or("").to_string();
        let port = body.get("port").and_then(|p| p.as_u64()).unwrap_or(1883);
        let key = format!("{host}:{port}");
        let mut favs = self.favorites.lock().await;
        favs.retain(|f| f.get("key").and_then(|k| k.as_str()) != Some(&key));
        let mut fav = serde_json::Value::Object(serde_json::Map::new());
        for f in ["host", "port", "callsign", "name", "uid", "cert", "online", "total"] {
            if let Some(v) = body.get(f) {
                if !v.is_null() {
                    fav[f] = v.clone();
                }
            }
        }
        fav["key"] = json!(key);
        fav["favorited_at"] = json!(chrono::Utc::now().timestamp());
        favs.push(fav.clone());
        self.save_favorites(&favs);
        drop(favs);
        (self.emit)(json!({"type": "favorites", "favorites": self.favorites.lock().await.clone()}));
        fav
    }

    pub async fn favorites_remove(&self, key: &str) {
        let mut favs = self.favorites.lock().await;
        favs.retain(|f| f.get("key").and_then(|k| k.as_str()) != Some(&key));
        self.save_favorites(&favs);
        drop(favs);
        (self.emit)(json!({"type": "favorites", "favorites": self.favorites.lock().await.clone()}));
    }

    fn save_favorites(&self, list: &[serde_json::Value]) {
        if let Ok(text) = serde_json::to_string_pretty(list) {
            std::fs::write(&self.favorites_path, text).ok();
        }
    }

    /// FMO 独立统计快照（供前端第二栏展示）。
    pub async fn stats_snapshot(&self) -> serde_json::Value {
        let mut out = self.stats.snapshot();
        out["callsign"] = json!(self.current_callsign());
        out["uid"] = json!(self.current_uid());
        out["mqttState"] = json!(self.mqtt_client.state_str().await);
        out["mqttDetail"] = json!(self.mqtt_client.detail.lock().await.clone());
        out["aprsState"] = json!(self.aprs_client.state.lock().await.clone());
        out["aprsDetail"] = json!(self.aprs_client.detail.lock().await.clone());
        let sel = self.selected_server.lock().await.clone();
        let host = sel.get("host").and_then(|h| h.as_str()).unwrap_or("").to_string();
        let port = sel.get("port").and_then(|p| p.as_u64()).unwrap_or(0);
        out["serverHost"] = json!(host);
        out["serverPort"] = json!(port);
        // 服务器名称：优先选中项 name；缺失时从服务器表按 host 补齐
        let mut server_name = sel
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        if server_name.is_empty() && !host.is_empty() {
            for s in self.server_table.to_list().await {
                if s.get("host").and_then(|h| h.as_str()) == Some(&host) {
                    if let Some(n) = s.get("name").and_then(|n| n.as_str()) {
                        if !n.is_empty() {
                            server_name = n.to_string();
                        }
                    }
                    break;
                }
            }
        }
        out["serverName"] = json!(server_name);
        out["activeSpeaker"] = json!(self.current_speaker.lock().unwrap().clone());
        out
    }
}
