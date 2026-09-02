//! FMO 运行时状态：证书库 / APRS / MQTT / 收发音频 / 收藏的统一编排。
//!
//! 事件通道：所有内部 emit 通过 `fmo://event` 发送 JSON（含 `type` 字段），
//! 与 sim-rust WS 协议保持一致，前端单一 listener 分发。

use crate::fmo::aprs::{AprsClient, AprsParams, EmitFn, ServerTable};
use crate::fmo::audio::{RxAudio, TxSession};
use crate::fmo::broadcast::{BeaconEngine, BroadcastEngine};
use crate::fmo::certstore::CertStore;
use crate::fmo::fmo_auth;
use crate::fmo::fmo_frame;
use crate::fmo::mqtt_client::FmoMqttClient;
use crate::fmo::presence::PresenceTracker;
use crate::fmo::qso::QsoEngine;
use crate::fmo::{mqtt_client as fmo_mqtt, qso as fmo_qso};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex;

pub const DEFAULT_APRS_HOST: &str = "rotate.aprs2.net";
pub const DEFAULT_APRS_PORT: u16 = 10152;

/// 读取当前身份（呼号, uid）：configured 非空优先作呼号，否则取 cert_user.json。
pub fn read_identity(data_dir: &Path, configured: &str) -> (String, u32) {
    let mut callsign = String::new();
    let mut uid = 0u32;
    let p = data_dir.join("certs").join("cert_user.json");
    if let Ok(text) = std::fs::read_to_string(&p) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(cs) = v["subject"]["callsign"].as_str() {
                callsign = cs.to_string();
            }
            if let Some(u) = v["subject"]["uid"].as_u64() {
                uid = u as u32;
            }
        }
    }
    if !configured.trim().is_empty() {
        callsign = configured.trim().to_string();
    }
    if callsign.is_empty() {
        callsign = "N0CALL".into();
    }
    (callsign, uid)
}

/// QSO 成员网格表条目（FMO/QSO/UID/# 成员 JSON 实时推送，对齐原厂固件：
/// 网格随 isSpeaking 发言状态发布，是通话中对方位置的实时来源）。
#[derive(Clone, Default)]
pub struct MemberEntry {
    pub grid: String,
    pub is_speaking: bool,
    pub is_host: bool,
    pub ts: i64,
}

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
    /// 语音互转（桥接）发射会话：独立于 PTT 的 tx_session，避免互相干扰。
    /// 由 runtime 的空闲看门狗在 800ms 无帧后关闭。
    pub bridge_tx: Arc<Mutex<Option<Arc<TxSession>>>>,
    /// QSO 呼叫信令引擎（APRS APFMO0）
    pub qso: Arc<QsoEngine>,
    /// 服务器广播引擎（APRS APFMO4 STATION）
    pub broadcast: Arc<BroadcastEngine>,
    /// 个人信标引擎（APRS APFMO4 BEACON + APFMO2/APFMO1 跟发）
    pub beacon: Arc<BeaconEngine>,
    /// 在线数/峰值自动统计花名册（LATE 心跳，持久化 presence.json）
    pub presence: Arc<PresenceTracker>,
    pub rx_play_enabled: Arc<std::sync::Mutex<bool>>,
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
    /// 成员网格表（成员 JSON）：呼号大写 → 条目；呼号框距离/方位的数据源
    pub member_roster: Arc<std::sync::Mutex<HashMap<String, MemberEntry>>>,
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
        let bridge = Arc::new(std::sync::Mutex::new(
            None::<Arc<dyn Fn(serde_json::Value) + Send + Sync>>,
        ));
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
        let stats = FmoStats::default();
        // 顶栏"遥测/文本"计数挂到 MQTT 客户端的全局计数器
        let mut mqtt_client_inner = FmoMqttClient::new(emit.clone());
        mqtt_client_inner.cnt_tele = stats.server_info.clone();
        mqtt_client_inner.cnt_text = stats.rx_text.clone();
        // 在线数/峰值花名册：注入 MQTT 客户端（LATE 心跳维护、断线清空）与广播引擎（自动值）
        let presence = Arc::new(PresenceTracker::new(data_dir.join("presence.json")));
        mqtt_client_inner.set_presence(presence.clone());
        let mqtt_client = Arc::new(mqtt_client_inner);
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
        let selected_server = Arc::new(Mutex::new(selected));
        let configured_callsign = Arc::new(std::sync::Mutex::new(String::new()));

        // QSO 信令引擎 + 服务器广播引擎（共用 APRS 上行连接）
        let qso = Arc::new(QsoEngine::new(
            emit.clone(),
            aprs_client.tx.clone(),
            server_table.clone(),
            selected_server.clone(),
            data_dir.clone(),
            configured_callsign.clone(),
        ));
        let broadcast = Arc::new(BroadcastEngine::new(
            emit.clone(),
            aprs_client.tx.clone(),
            data_dir.clone(),
            mqtt_client.state.clone(),
            mqtt_client.current_role.clone(),
            selected_server.clone(),
            presence.clone(),
        ));
        // 个人信标引擎（位置复用广播配置的经纬度）；回注广播引擎用于 APFMO1 公告跟发
        let beacon = Arc::new(BeaconEngine::new(
            emit.clone(),
            aprs_client.tx.clone(),
            data_dir.clone(),
            broadcast.cfg_handle(),
        ));
        broadcast.set_beacon(beacon.clone());
        // APRS 信令消息 → QSO 引擎（主全馈连接与上行连接都会投递，引擎内去重）
        {
            let qso_handler = qso.clone();
            *aprs_client.on_message.lock().unwrap() = Some(Arc::new(move |ev| {
                let qso_handler = qso_handler.clone();
                tauri::async_runtime::spawn(async move {
                    qso_handler.handle_message(ev).await;
                });
            }));
        }

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
            bridge_tx: Arc::new(Mutex::new(None)),
            qso,
            broadcast,
            beacon,
            presence,
            rx_play_enabled: Arc::new(std::sync::Mutex::new(true)),
            selected_server,
            favorites: Arc::new(Mutex::new(favorites)),
            favorites_path,
            current_speaker: Arc::new(std::sync::Mutex::new(String::new())),
            bridge,
            stats,
            configured_callsign,
            member_roster: Arc::new(std::sync::Mutex::new(HashMap::new())),
            aprs_task_running: Arc::new(AtomicBool::new(false)),
            identity_watch_running: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn set_app(&self, app: AppHandle) {
        *self.app.lock().unwrap() = Some(app);
    }

    pub fn current_callsign(&self) -> String {
        let configured = self.configured_callsign.lock().unwrap().clone();
        read_identity(&self.data_dir, &configured).0
    }

    pub fn current_uid(&self) -> u32 {
        let configured = self.configured_callsign.lock().unwrap().clone();
        read_identity(&self.data_dir, &configured).1
    }

    /// QSO 跳台钩子：收到 QTHANS 后按 S<服务器uid> 查表切服务器（主叫跳到被叫服务器）。
    pub fn install_qso_jump_hook(self: &Arc<Self>) {
        let this = self.clone();
        self.qso.install_jump_hook(Arc::new(move |srv_uid: u32| {
            let this = this.clone();
            tauri::async_runtime::spawn(async move {
                let Some(entry) = this.server_table.find_server_by_uid(srv_uid).await else {
                    (this.emit)(json!({"type": "log", "level": "warn",
                        "msg": format!("QSO 跳台：服务器表里没有 uid={srv_uid} 的条目（等它的 STATION 广播后重试）")}));
                    return;
                };
                let host = entry.get("host").and_then(|h| h.as_str()).unwrap_or("").to_string();
                // select_server now reconnects MQTT itself when connected/connecting,
                // so the hook only selects and logs.
                this.select_server(entry).await;
                (this.emit)(json!({"type": "log", "level": "info",
                    "msg": format!("QSO 跳台：已选定对方服务器 {host}")}));
            });
        }));
    }

    /// QSO 祝福（qso_best_wish）钩子：
    /// - 发送：QSO 建立（qso 引擎 Established 单一挂点）时，把完整通联记录 JSON
    ///   （祝福在 toComment）发布到对方 `FMO/QSO/UID/<对方uid>`；
    /// - 接收：MQTT 收到 `FMO/QSO/UID/<本机uid>` 的完整记录 JSON 时写入本地 qso_log，
    ///   前端 QSO 记录展示祝福（toComment）。
    pub fn install_qso_wish_hooks(self: &Arc<Self>) {
        let this = self.clone();
        self.qso
            .install_established_hook(Arc::new(move |peer: String, peer_uid: u32| {
                let this = this.clone();
                tauri::async_runtime::spawn(async move {
                    this.publish_qso_record(&peer, peer_uid).await;
                });
            }));
        let this = self.clone();
        *self.mqtt_client.on_qso_record.lock().unwrap() = Some(Arc::new(
            move |topic: String, payload: Vec<u8>| {
                let this = this.clone();
                tauri::async_runtime::spawn(async move {
                    this.handle_qso_record(&topic, &payload).await;
                });
            },
        ));
    }

    /// QSO 建立后向 `FMO/QSO/UID/<对方uid>` 发布完整通联记录 JSON。
    /// 发送失败（MQTT 未连接等）只记 warn 日志，不影响通联本身。
    async fn publish_qso_record(&self, peer: &str, peer_uid: u32) {
        if peer_uid == 0 {
            (self.emit)(json!({"type": "log", "level": "warn",
                "msg": format!("QSO 祝福未发送：不知道 {peer} 的 UID")}));
            return;
        }
        let beacon = self.beacon.config().await;
        let bc = self.broadcast.config().await;
        let grid = fmo_qso::maidenhead_grid(bc.lat, bc.lon);
        // 无射频：频率用 beacon 配置的直频（未配置为 0）
        let freq_hz = if beacon.freq_mhz > 0.0 {
            (beacon.freq_mhz * 1e6) as u64
        } else {
            0
        };
        let sel = self.selected_server.lock().await.clone();
        let relay_name = sel
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        let relay_admin = sel
            .get("callsign")
            .and_then(|c| c.as_str())
            .or_else(|| {
                sel.get("cert")
                    .and_then(|c| c.get("callsign"))
                    .and_then(|c| c.as_str())
            })
            .unwrap_or("")
            .to_string();
        let my_call = self.current_callsign();
        // logId = 刚写入的本地 qso_log 条目序号（Established 前 record() 已追加）
        let log_id = self.qso.qso_log().len() as u32;
        let rec = fmo_qso::build_qso_record(
            log_id,
            chrono::Utc::now().timestamp() as u64,
            freq_hz,
            &my_call,
            &grid,
            peer,
            "",
            &beacon.qso_msg,
            "FMO",
            &relay_name,
            &relay_admin,
        );
        let topic = format!("FMO/QSO/UID/{peer_uid}");
        let payload = rec.to_string().into_bytes();
        match self.mqtt_client.publish(&topic, payload, 0).await {
            Ok(()) => {
                let wish = if beacon.qso_msg.is_empty() {
                    String::new()
                } else {
                    format!("，祝福：{}", beacon.qso_msg)
                };
                (self.emit)(json!({"type": "log", "level": "info",
                    "msg": format!("QSO 通联记录已发送给 {peer}（{topic}）{wish}")}));
            }
            Err(e) => {
                (self.emit)(json!({"type": "log", "level": "warn",
                    "msg": format!("QSO 通联记录发送失败（{topic}）：{e}")}));
            }
        }
    }

    /// 收到 `FMO/QSO/UID/<uid>` 载荷：两种载荷分开处理——
    /// 1) 完整通联记录 JSON：只收发给自己的（topic 尾段 == 本机 uid），写入本地 qso_log；
    /// 2) 成员 JSON（{"callsign","isSpeaking","isHost","grid"}）：存入成员网格表，
    ///    供呼号框实时距离/方位显示（对齐原厂：网格随发言状态推送，不限本机 topic）。
    async fn handle_qso_record(&self, topic: &str, payload: &[u8]) {
        if let Some(rec) = fmo_mqtt::parse_qso_record_json(payload) {
            let my_uid = self.current_uid();
            let topic_uid: u32 = topic
                .rsplit('/')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if my_uid == 0 || topic_uid != my_uid {
                return;
            }
            let from = rec
                .get("fromCallsign")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let comment = rec
                .get("toComment")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            self.qso.record_remote(&rec);
            let wish = if comment.is_empty() {
                String::new()
            } else {
                format!("，祝福：{comment}")
            };
            (self.emit)(json!({"type": "log", "level": "info",
                "msg": format!("收到 {from} 的 QSO 通联记录{wish}")}));
            return;
        }
        if let Some(m) = fmo_mqtt::parse_qso_member_json(payload) {
            let cs = m
                .get("callsign")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .trim()
                .to_uppercase();
            if cs.is_empty() {
                return;
            }
            let entry = MemberEntry {
                grid: m
                    .get("grid")
                    .and_then(|g| g.as_str())
                    .unwrap_or("")
                    .to_string(),
                is_speaking: m
                    .get("isSpeaking")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                is_host: m.get("isHost").and_then(|v| v.as_bool()).unwrap_or(false),
                ts: chrono::Utc::now().timestamp(),
            };
            let mut roster = self.member_roster.lock().unwrap();
            roster.insert(cs, entry);
            // 上限保护：超过 500 个成员淘汰最久未更新的
            if roster.len() > 500 {
                let oldest = roster
                    .iter()
                    .min_by_key(|(_, e)| e.ts)
                    .map(|(k, _)| k.clone());
                if let Some(k) = oldest {
                    roster.remove(&k);
                }
            }
        }
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
            srv["uid"] = cert_info
                .get("uid")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
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
        let ok_host = srv
            .get("host")
            .and_then(|h| h.as_str())
            .map(|h| !h.is_empty())
            .unwrap_or(false);
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
        // Self-heal: refresh the selection from server_table first, so a server
        // cert rotation seen via STATION broadcast reaches the SAS credentials.
        self.refresh_selected_from_table().await;
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
        let host = sel
            .get("host")
            .and_then(|h| h.as_str())
            .unwrap_or("")
            .to_string();
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
            let fp_hex = srv["fingerprint"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .take(8)
                        .filter_map(|b| b.as_u64())
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
                })
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
        let username = creds
            .get("username")
            .and_then(|u| u.as_str())
            .map(|s| s.to_string());
        let password = creds
            .get("password")
            .and_then(|p| p.as_str())
            .map(|s| s.to_string());
        let role = creds
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("user")
            .to_string();
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
        if self
            .aprs_task_running
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let client = self.aprs_client.clone();
        tauri::async_runtime::spawn(async move {
            client.run().await;
        });
        // APRS 上行（发送专用）连接
        let tx = self.aprs_client.tx.clone();
        tauri::async_runtime::spawn(async move {
            tx.run().await;
        });
        // QSO 超时 tick + 服务器自动广播 + 个人信标周期循环
        self.qso.start_tick_task();
        self.broadcast.start();
        self.beacon.start();
    }

    /// 证书身份巡检：启动即查一次，之后每 10 分钟复查。
    /// 证书后来过期、私钥与证书不配套、临近 7 天到期都会以 warn 日志提醒
    /// （状态不变不重复提醒，修复后再次出现问题会重新提醒）。
    pub fn start_identity_watchdog(self: &Arc<Self>) {
        if self
            .identity_watch_running
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
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
        let speaker = self.current_speaker.clone();
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

    /// 更新选定服务器并持久化（下次启动恢复，用于自动连接）。
    async fn update_selected(&self, server: serde_json::Value) {
        {
            let mut sel = self.selected_server.lock().await;
            *sel = server.clone();
        }
        if let Ok(text) = serde_json::to_string_pretty(&server) {
            std::fs::write(self.data_dir.join("selected_server.json"), text).ok();
        }
    }

    /// 选定服务器并持久化（下次启动恢复，用于自动连接）。
    /// 若当前 MQTT 已连接或正在连接，断开并按新选定项重连，
    /// 使所有 UI 入口（服务器列表 / 收藏行 / QSO 跳台）行为一致。
    pub async fn select_server(&self, server: serde_json::Value) {
        self.update_selected(server).await;
        // Reconnect outside any lock: connect_mqtt re-locks selected_server internally.
        let st = self.mqtt_client.state_str().await;
        if st == "connected" || st == "connecting" {
            self.disconnect_mqtt().await;
            if let Err(e) = self.connect_mqtt(false).await {
                (self.emit)(json!({"type": "log", "level": "error",
                    "msg": format!("切换服务器后重连 MQTT 失败：{e}")}));
            }
        }
    }

    /// 在 server_table 里查选定服务器对应的最新条目（uid 优先，其次 host:port）。
    async fn table_entry_for_selected(
        &self,
        sel: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        if let Some(uid) = server_uid_of(sel) {
            if let Some(e) = self.server_table.find_server_by_uid(uid).await {
                return Some(e);
            }
        }
        let host = sel.get("host").and_then(|h| h.as_str()).unwrap_or("");
        if host.is_empty() {
            return None;
        }
        let port = sel.get("port").and_then(|p| p.as_u64()).unwrap_or(1883);
        self.server_table
            .to_list()
            .await
            .into_iter()
            .find(|e| {
                e.get("host").and_then(|h| h.as_str()) == Some(host)
                    && e.get("port").and_then(|p| p.as_u64()).unwrap_or(1883) == port
            })
    }

    /// 自愈刷新：server_table 里同一台服务器的证书指纹若与选定项不一致（服务器
    /// 已换新证而本地缓存还是旧证），用表里的最新 cert 更新 selected_server 并
    /// 持久化。返回是否刷新。
    async fn refresh_selected_from_table(&self) -> bool {
        let sel = self.selected_server.lock().await.clone();
        if sel.is_null() {
            return false;
        }
        let Some(entry) = self.table_entry_for_selected(&sel).await else {
            return false;
        };
        let Some(new_sel) = refreshed_selection(&sel, &entry) else {
            return false;
        };
        (self.emit)(json!({"type": "log", "level": "info",
            "msg": "选定服务器证书已更新（STATION 广播），已用新指纹刷新选定项"}));
        self.update_selected(new_sel).await;
        true
    }

    /// STATION 广播 upsert 回调：广播的是当前选定服务器且证书指纹变了时刷新选定项；
    /// MQTT 正连着这台服务器（connected/connecting 且 current_host 匹配）则断开重连。
    async fn maybe_refresh_selected(&self, entry: serde_json::Value) {
        let sel = self.selected_server.lock().await.clone();
        if sel.is_null() {
            return;
        }
        let Some(new_sel) = refreshed_selection(&sel, &entry) else {
            return;
        };
        let st = self.mqtt_client.state_str().await;
        let cur_host = self.mqtt_client.current_host.lock().await.clone();
        let host = new_sel
            .get("host")
            .and_then(|h| h.as_str())
            .unwrap_or("")
            .to_string();
        (self.emit)(json!({"type": "log", "level": "info",
            "msg": format!("选定服务器 {host} 证书已更新（STATION 广播），已刷新选定项")}));
        if (st == "connected" || st == "connecting") && !host.is_empty() && cur_host == host {
            // select_server persists and reconnects when connected/connecting.
            self.select_server(new_sel).await;
        } else {
            self.update_selected(new_sel).await;
        }
    }

    /// 安装 server_table upsert 钩子（STATION 广播在线自愈刷新）。
    pub fn install_server_refresh_hook(self: &Arc<Self>) {
        let this = self.clone();
        *self.server_table.on_upsert.lock().unwrap() = Some(Arc::new(move |entry| {
            let this = this.clone();
            tauri::async_runtime::spawn(async move {
                this.maybe_refresh_selected(entry).await;
            });
        }));
    }

    /// 默认选定一台带证书信息的在线服务器（已有选定项时不覆盖）。
    pub async fn select_default_server(&self) {
        if !self.selected_server.lock().await.is_null() {
            return;
        }
        let list = self.server_table.to_list().await;
        let mut cands: Vec<serde_json::Value> = list
            .into_iter()
            .filter(|s| {
                s.get("host")
                    .and_then(|h| h.as_str())
                    .map(|h| !h.is_empty())
                    .unwrap_or(false)
                    && s.get("cert").and_then(|c| c.get("uid")).is_some()
            })
            .collect();
        cands.sort_by_key(|s| {
            std::cmp::Reverse(s.get("online").and_then(|o| o.as_i64()).unwrap_or(0))
        });
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

    /// 桥接喂 PCM（8k s16le）。返回 true 表示本次新建了发射会话，
    /// 调用方需启动空闲看门狗在无帧时调 bridge_stop_tx 关闭会话。
    pub async fn bridge_feed_pcm(&self, pcm: &[i16], mode: &str) -> bool {
        if self.mqtt_client.state_str().await != "connected" {
            return false;
        }
        if let Some(ts) = self.bridge_tx.lock().await.clone() {
            ts.feed_pcm(pcm).await;
            return false;
        }
        let callsign = self.current_callsign();
        let Ok(ts) = TxSession::new(
            self.mqtt_client.clone(),
            &callsign,
            mode,
            Some(self.stats.tx_frames.clone()),
        ) else {
            return false;
        };
        let ts = Arc::new(ts);
        ts.feed_pcm(pcm).await;
        *self.bridge_tx.lock().await = Some(ts);
        true
    }

    /// 桥接发射会话结束（空闲看门狗或模式切换时调用）。
    pub async fn bridge_stop_tx(&self) {
        let tx = self.bridge_tx.lock().await.clone();
        if let Some(ts) = tx {
            ts.stop().await;
        }
        *self.bridge_tx.lock().await = None;
    }

    // ---------------------------------------------------------------- 收藏

    pub async fn favorites_list(&self) -> Vec<serde_json::Value> {
        self.favorites.lock().await.clone()
    }

    pub async fn favorites_add(&self, body: serde_json::Value) -> serde_json::Value {
        let host = body
            .get("host")
            .and_then(|h| h.as_str())
            .unwrap_or("")
            .to_string();
        let port = body.get("port").and_then(|p| p.as_u64()).unwrap_or(1883);
        let key = format!("{host}:{port}");
        let mut favs = self.favorites.lock().await;
        favs.retain(|f| f.get("key").and_then(|k| k.as_str()) != Some(&key));
        let mut fav = serde_json::Value::Object(serde_json::Map::new());
        for f in [
            "host", "port", "callsign", "name", "uid", "cert", "online", "total",
        ] {
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
        out["mqttClientId"] = json!(self.mqtt_client.client_id_str().await);
        out["mqttRole"] = json!(self.mqtt_client.current_role.lock().await.clone());
        out["aprsState"] = json!(self.aprs_client.state.lock().await.clone());
        out["aprsDetail"] = json!(self.aprs_client.detail.lock().await.clone());
        let sel = self.selected_server.lock().await.clone();
        let host = sel
            .get("host")
            .and_then(|h| h.as_str())
            .unwrap_or("")
            .to_string();
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
        // 广播配置提前取出：说话人位置计算（本机 QTH）与 online/peak 生效值都要用
        let bc_cfg = self.broadcast.config().await;
        // 说话人位置/距离/方位（对齐原厂固件：Position 类大圆计算 + 16 方位罗盘）。
        // 位置来源优先级：APRS 用户表经纬度（精确，source=beacon）→ 成员 JSON 网格
        // （方格中心 ±10km，source=grid，前端标 ≈）。本机位置用广播配置的 QTH。
        {
            let spk = self.current_speaker.lock().unwrap().clone();
            let own_ok = bc_cfg.lat.is_finite()
                && bc_cfg.lon.is_finite()
                && bc_cfg.lat.abs() <= 90.0
                && bc_cfg.lon.abs() <= 180.0;
            if !spk.is_empty() && own_ok {
                let mut geo: Option<(f64, f64, &'static str)> = None;
                // 1) APRS 用户表（信标经纬度，精确）
                if let Some(c) = self.server_table.find_client(&spk).await {
                    let la = c
                        .get("lat")
                        .and_then(|v| v.as_str())
                        .and_then(crate::fmo::aprs::aprs_to_deg);
                    let lo = c
                        .get("lon")
                        .and_then(|v| v.as_str())
                        .and_then(crate::fmo::aprs::aprs_to_deg);
                    if let (Some(la), Some(lo)) = (la, lo) {
                        geo = Some((la, lo, "beacon"));
                    }
                }
                // 2) 成员 JSON 网格（±10km）
                if geo.is_none() {
                    let key = spk.to_uppercase();
                    let base = key.split('-').next().unwrap_or(&key).to_string();
                    let grid = {
                        let roster = self.member_roster.lock().unwrap();
                        roster
                            .get(&key)
                            .or_else(|| roster.get(&base))
                            .map(|e| e.grid.clone())
                    };
                    if let Some(g) = grid {
                        if let Some((la, lo)) = fmo_qso::grid_to_latlon(&g) {
                            geo = Some((la, lo, "grid"));
                        }
                    }
                }
                if let Some((la, lo, src)) = geo {
                    let (dist_m, brg) =
                        fmo_qso::geo_distance_bearing(bc_cfg.lat, bc_cfg.lon, la, lo);
                    // 与原厂一致：(米+500)/1000 取整 → 整数公里
                    let km = ((dist_m + 500.0) / 1000.0).floor() as u32;
                    out["speakerDistanceKm"] = json!(km);
                    out["speakerBearingDeg"] = json!((brg * 10.0).round() / 10.0);
                    out["speakerCompass"] = json!(fmo_qso::compass16(brg));
                    out["speakerPosSource"] = json!(src);
                    out["speakerGrid"] = json!(fmo_qso::maidenhead_grid(la, lo));
                }
            }
        }
        // 在线数/峰值自动统计（LATE 心跳花名册）+ 广播 online/peak 生效值（0=自动，>0=手动覆盖）
        out["presenceOnline"] = json!(self.presence.online());
        out["presencePeak"] = json!(self.presence.peak());
        let (eff_online, eff_peak) = self.broadcast.effective_online_peak(&bc_cfg);
        out["broadcastOnline"] = json!(eff_online);
        out["broadcastPeak"] = json!(eff_peak);
        // 个人信标（BEACON）状态
        out["beaconEnabled"] = json!(self.beacon.config().await.enabled);
        out["beaconLastSent"] = json!(self.beacon.last_sent().await);
        out
    }
}

/// 取服务器条目的 uid（顶层 uid 缺失时回落到 cert.uid）。
fn server_uid_of(v: &serde_json::Value) -> Option<u32> {
    v.get("uid")
        .and_then(|u| u.as_u64())
        .or_else(|| {
            v.get("cert")
                .and_then(|c| c.get("uid"))
                .and_then(|u| u.as_u64())
        })
        .map(|u| u as u32)
}

/// 若 entry 与 sel 是同一台服务器（uid 匹配优先，其次 host:port）且 cert 指纹
/// 不同（服务器换了新证，本地选定项还是旧证），返回用 entry 重建的选定项 JSON；
/// 否则 None。指纹含 iat，指纹不同即覆盖「iat 更新」的情形。
fn refreshed_selection(
    sel: &serde_json::Value,
    entry: &serde_json::Value,
) -> Option<serde_json::Value> {
    let same_server = match (server_uid_of(sel), server_uid_of(entry)) {
        (Some(a), Some(b)) => a == b,
        _ => {
            let h1 = sel.get("host").and_then(|h| h.as_str()).unwrap_or("");
            let h2 = entry.get("host").and_then(|h| h.as_str()).unwrap_or("");
            !h1.is_empty()
                && h1 == h2
                && sel.get("port").and_then(|p| p.as_u64()).unwrap_or(1883)
                    == entry.get("port").and_then(|p| p.as_u64()).unwrap_or(1883)
        }
    };
    if !same_server {
        return None;
    }
    let new_cert = entry.get("cert").filter(|c| c.is_object())?;
    let fp_new = fmo_auth::beacon_cert_fingerprint(new_cert);
    if let Some(old_cert) = sel.get("cert").filter(|c| c.is_object()) {
        if fmo_auth::beacon_cert_fingerprint(old_cert) == fp_new {
            return None;
        }
    }
    Some(json!({
        "host": entry.get("host").cloned().unwrap_or(json!("")),
        "port": entry.get("port").cloned().unwrap_or(json!(1883)),
        "callsign": new_cert.get("callsign").cloned().unwrap_or(json!("")),
        "uid": new_cert.get("uid").cloned().unwrap_or(serde_json::Value::Null),
        "cert": new_cert.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cert_json(pubkey_byte: u8, iat: u64) -> serde_json::Value {
        cert_with_uid(pubkey_byte, iat, Some(26658))
    }

    fn cert_with_uid(pubkey_byte: u8, iat: u64, uid: Option<u64>) -> serde_json::Value {
        let mut c = json!({
            "alg": 1,
            "callsign": "BD4RFG",
            "pubkey_hex": hex::encode([pubkey_byte; 32]),
            "iat": iat,
            "exp": iat + 86400,
        });
        if let Some(u) = uid {
            c["uid"] = json!(u);
        }
        c
    }

    #[test]
    fn refresh_selection_picks_new_fingerprint() {
        // 选定项存旧证、表里是新证：应刷新为新指纹
        let sel = json!({
            "host": "china.fmocq.com", "port": 8883,
            "callsign": "BD4RFG", "uid": 26658, "cert": cert_json(0x11, 1000),
        });
        let entry = json!({
            "host": "china.fmocq.com", "port": 8883,
            "uid": 26658, "cert": cert_json(0x22, 2000),
        });
        let new_sel = refreshed_selection(&sel, &entry).expect("证书指纹变了应刷新");
        assert_eq!(
            fmo_auth::beacon_cert_fingerprint(&new_sel["cert"]),
            fmo_auth::beacon_cert_fingerprint(&entry["cert"]),
        );
        assert_eq!(new_sel["cert"]["iat"].as_u64(), Some(2000));
        assert_eq!(new_sel["host"].as_str(), Some("china.fmocq.com"));
    }

    #[test]
    fn refresh_selection_skips_same_fingerprint() {
        let cert = cert_json(0x11, 1000);
        let sel = json!({"host": "h", "port": 1883, "uid": 26658, "cert": cert});
        let entry = json!({"host": "h", "port": 1883, "uid": 26658, "cert": cert});
        assert!(refreshed_selection(&sel, &entry).is_none());
    }

    #[test]
    fn refresh_selection_skips_other_server() {
        // uid 不同的服务器不刷新（cert 里的 uid 也不同）
        let sel = json!({"host": "a", "port": 1883, "uid": 1, "cert": cert_with_uid(0x11, 1000, Some(1))});
        let entry = json!({"host": "a", "port": 1883, "uid": 2, "cert": cert_with_uid(0x22, 2000, Some(2))});
        assert!(refreshed_selection(&sel, &entry).is_none());
        // 无 uid 时按 host:port 匹配，host 不同不刷新
        let sel = json!({"host": "a", "port": 1883, "cert": cert_with_uid(0x11, 1000, None)});
        let entry = json!({"host": "b", "port": 1883, "cert": cert_with_uid(0x22, 2000, None)});
        assert!(refreshed_selection(&sel, &entry).is_none());
    }

    #[test]
    fn refresh_selection_matches_by_host_port_without_uid() {
        // 双方都没有 uid（顶层与 cert 都没有）时按 host:port 判定同一台服务器
        let sel = json!({"host": "h", "port": 1883, "cert": cert_with_uid(0x11, 1000, None)});
        let entry = json!({"host": "h", "port": 1883, "cert": cert_with_uid(0x22, 2000, None)});
        assert!(refreshed_selection(&sel, &entry).is_some());
    }

    #[test]
    fn refresh_selection_skips_entry_without_cert() {
        let sel = json!({"host": "h", "port": 1883, "uid": 26658, "cert": cert_json(0x11, 1000)});
        let entry = json!({"host": "h", "port": 1883, "uid": 26658});
        assert!(refreshed_selection(&sel, &entry).is_none());
    }
}
