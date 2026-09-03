//! FMO QSO 呼叫信令引擎（APRS APFMO0 消息）。
//!
//! 协议逆向自原厂固件（fmo-sim/docs/firmware-analysis.md §8.2）：
//! - 报文：`:目标呼号<补齐9>:<动词>,…{msgId`，AX.25 目的 APFMO0、路径 TCPIP*
//! - QTHQRY,Q<本机uid>,U<目标uid> → 被查询方自动回 QTHANS,F1,U<uid>,S<服务器uid>,LA<UTC时间>,<GBK服务器名>
//! - CALL,Q<本机uid>,U<目标uid>,S<目标服务器uid>,<服务器名> → 被叫回 CALLANS,RING，人工/自动 ACCEPT 或 REJECT
//! - 超时：QTHQRY 10s（每 3s 重发，递增 msgId，与实捕 {2 {3 {4 一致）；
//!   CALL 等 RING 7s；等 ACCEPT 60s；被叫振铃 60s
//! - 接通语义：主叫跳到被叫服务器（QTHANS 的 S → 查服务器表 → 切 MQTT），被叫不动

use crate::fmo::aprs::{AprsTx, EmitFn, ServerTable};
use serde_json::json;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

const QUERY_TIMEOUT_S: i64 = 10;
const QUERY_RETRY_S: i64 = 3;
const RING_TIMEOUT_S: i64 = 7;
const ACCEPT_TIMEOUT_S: i64 = 60;
const IN_RING_TIMEOUT_S: i64 = 60;

#[derive(Clone, Debug, PartialEq)]
enum OutStage {
    WaitRing,
    WaitAccept,
}

#[derive(Clone, Debug)]
enum QsoPhase {
    Idle,
    /// 已发 QTHQRY，等 QTHANS
    OutQuery {
        peer: String,
        peer_uid: u32,
        started: i64,
        last_sent: i64,
        deadline: i64,
    },
    /// 已发 CALL，等 RING / ACCEPT
    OutCall {
        peer: String,
        peer_uid: u32,
        srv_uid: u32,
        srv_name: String,
        stage: OutStage,
        deadline: i64,
    },
    /// 收到 CALL，振铃中（等用户接听/拒绝；自动接受时不会进入此状态）
    InRing {
        peer: String,
        peer_uid: u32,
        srv_uid: u32,
        deadline: i64,
    },
    /// 已接通
    Established {
        peer: String,
        peer_uid: u32,
        since: i64,
        outgoing: bool,
    },
}

pub struct QsoEngine {
    emit: EmitFn,
    tx: Arc<AprsTx>,
    table: Arc<ServerTable>,
    selected_server: Arc<Mutex<serde_json::Value>>,
    data_dir: PathBuf,
    configured_callsign: Arc<std::sync::Mutex<String>>,
    phase: Arc<Mutex<QsoPhase>>,
    auto_accept: Arc<std::sync::Mutex<bool>>,
    seq: Arc<AtomicU32>,
    /// 原始报文去重（主全馈连接与上行连接都会收到同一条信令）
    seen: Arc<std::sync::Mutex<VecDeque<String>>>,
    /// 跳服务器钩子（FmoState 安装）：参数 = 目标服务器 uid
    jump_hook: Arc<std::sync::Mutex<Option<Arc<dyn Fn(u32) + Send + Sync>>>>,
    /// QSO 建立钩子（FmoState 安装，用于 QSO 祝福发布）：参数 = (对方呼号, 对方uid)。
    /// 单一挂点：仅 set_phase 进入 Established 时触发（接听/自动接受/对方 ACCEPT），
    /// 取消/拒接/超时不会触发，保证每次通联只发一次。
    established_hook: Arc<std::sync::Mutex<Option<Arc<dyn Fn(String, u32) + Send + Sync>>>>,
    cfg_path: PathBuf,
    log_path: PathBuf,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// 去掉 SSID 的大写基呼号
pub(crate) fn base_call(cs: &str) -> String {
    cs.split('-').next().unwrap_or(cs).trim().to_uppercase()
}

/// 经纬度 → 6 位梅登黑德网格（如 39.9,116.4 → OM89ev）。
/// 非法输入（NaN/无穷）返回空串；经纬度越界时收敛到合法范围。
pub fn maidenhead_grid(lat: f64, lon: f64) -> String {
    if !lat.is_finite() || !lon.is_finite() {
        return String::new();
    }
    // 经度归一到 [-180,180)，纬度收敛到 [-90,90]；再各留一点余量防浮点上溢
    let lo = ((lon + 180.0).rem_euclid(360.0) - 180.0 + 180.0).min(359.999999);
    let la = (lat.clamp(-90.0, 90.0) + 90.0).min(179.999999);
    let ch = |i: u32, base: u8| (base + i as u8) as char;
    let mut s = String::with_capacity(6);
    s.push(ch((lo / 20.0) as u32, b'A'));
    s.push(ch((la / 10.0) as u32, b'A'));
    s.push(ch(((lo % 20.0) / 2.0) as u32, b'0'));
    s.push(ch((la % 10.0) as u32, b'0'));
    s.push(ch((((lo % 2.0) / 2.0) * 24.0) as u32, b'a'));
    s.push(ch(((la % 1.0) * 24.0) as u32, b'a'));
    s
}

/// 4/6 位梅登黑德网格 → 方格中心经纬度（与 maidenhead_grid 互逆的解码器，
/// 对齐原厂固件 Position::fromGrid @0x42080588 / 解码核 @0x42082084）。
/// 非法输入返回 None（原厂默认 (-180,-90)，UI 层宁可不显示）。
/// 精度：6 位方格 5′×2.5′，4 位方格 2°×1°，返回方格中心点。
pub fn grid_to_latlon(grid: &str) -> Option<(f64, f64)> {
    let g: Vec<u8> = grid.trim().to_uppercase().bytes().collect();
    if g.len() != 4 && g.len() != 6 {
        return None;
    }
    let field = |b: u8| -> Option<f64> {
        // 头两位：场（18×18 覆盖全球），字母范围 A..R
        if (b'A'..=b'R').contains(&b) {
            Some((b - b'A') as f64)
        } else {
            None
        }
    };
    let sub = |b: u8| -> Option<f64> {
        // 末两位：子方（24×24），字母范围 A..X（原厂固件 toupper 后同样按此范围）
        if (b'A'..=b'X').contains(&b) {
            Some((b - b'A') as f64)
        } else {
            None
        }
    };
    let dig = |b: u8| -> Option<f64> {
        if b.is_ascii_digit() {
            Some((b - b'0') as f64)
        } else {
            None
        }
    };
    let lon = field(g[0])? * 20.0 + dig(g[2])? * 2.0;
    let lat = field(g[1])? * 10.0 + dig(g[3])?;
    if g.len() == 4 {
        // 4 位方格 2°×1°，取中心
        Some((lat - 90.0 + 0.5, lon - 180.0 + 1.0))
    } else {
        // 6 位子方 5′×2.5′，取中心
        Some((
            lat - 90.0 + sub(g[5])? * 2.5 / 60.0 + 1.25 / 60.0,
            lon - 180.0 + sub(g[4])? * 5.0 / 60.0 + 2.5 / 60.0,
        ))
    }
}

/// 大圆距离（米）与初始方位角（度，0=北 顺时针）。
/// 与原厂固件同参数：f64、R=6371000 米、deg2rad/rad2deg（常量池 0x420808bc/c8/ec 实锤）。
pub fn geo_distance_bearing(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> (f64, f64) {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = (lat2 - lat1).to_radians();
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    let dist_m = 6_371_000.0 * 2.0 * a.sqrt().asin();
    let y = dl.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
    let brg = (y.atan2(x).to_degrees() + 360.0) % 360.0;
    (dist_m, brg)
}

/// 方位角 → 16 方位罗盘（与原厂字符串表一致：N/NNE/NE/ENE/E/ESE/SE/SSE/S/SSW/SW/WSW/W/WNW/NW/NNW）。
pub fn compass16(deg: f64) -> &'static str {
    const DIRS: [&str; 16] = [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W",
        "WNW", "NW", "NNW",
    ];
    DIRS[(((deg % 360.0) + 11.25) / 22.5).floor() as usize % 16]
}

/// 固件完整 QSO 记录 JSON（FMO/QSO/UID/<对方uid> 载荷，QSO 祝福在 toComment 字段）。
/// 字段名与固件模板一致：{"logId":..,"timestamp":..,"freqHz":..,"fromCallsign":..,
/// "fromGrid":..,"toCallsign":..,"toGrid":..,"toComment":..,"mode":..,"relayName":..,"relayAdmin":..}
#[allow(clippy::too_many_arguments)]
pub fn build_qso_record(
    log_id: u32,
    timestamp: u64,
    freq_hz: u64,
    from_callsign: &str,
    from_grid: &str,
    to_callsign: &str,
    to_grid: &str,
    to_comment: &str,
    mode: &str,
    relay_name: &str,
    relay_admin: &str,
) -> serde_json::Value {
    json!({
        "logId": log_id,
        "timestamp": timestamp,
        "freqHz": freq_hz,
        "fromCallsign": from_callsign,
        "fromGrid": from_grid,
        "toCallsign": to_callsign,
        "toGrid": to_grid,
        "toComment": to_comment,
        "mode": mode,
        "relayName": relay_name,
        "relayAdmin": relay_admin,
    })
}

fn gbk_bytes(s: &str) -> Vec<u8> {
    let (cow, _, _) = encoding_rs::GBK.encode(s);
    cow.into_owned()
}

impl QsoEngine {
    pub fn new(
        emit: EmitFn,
        tx: Arc<AprsTx>,
        table: Arc<ServerTable>,
        selected_server: Arc<Mutex<serde_json::Value>>,
        data_dir: PathBuf,
        configured_callsign: Arc<std::sync::Mutex<String>>,
    ) -> Self {
        let cfg_path = data_dir.join("qso_config.json");
        let auto_accept = std::fs::read_to_string(&cfg_path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v["autoAccept"].as_bool())
            .unwrap_or(false);
        Self {
            emit,
            tx,
            table,
            selected_server,
            data_dir: data_dir.clone(),
            configured_callsign,
            phase: Arc::new(Mutex::new(QsoPhase::Idle)),
            auto_accept: Arc::new(std::sync::Mutex::new(auto_accept)),
            seq: Arc::new(AtomicU32::new(1)),
            seen: Arc::new(std::sync::Mutex::new(VecDeque::new())),
            jump_hook: Arc::new(std::sync::Mutex::new(None)),
            established_hook: Arc::new(std::sync::Mutex::new(None)),
            cfg_path,
            log_path: data_dir.join("qso_log.json"),
        }
    }

    fn identity(&self) -> (String, u32) {
        let configured = self.configured_callsign.lock().unwrap().clone();
        crate::fmo::state::read_identity(&self.data_dir, &configured)
    }

    fn next_seq(&self) -> u32 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    pub fn install_jump_hook(&self, hook: Arc<dyn Fn(u32) + Send + Sync>) {
        *self.jump_hook.lock().unwrap() = Some(hook);
    }

    pub fn install_established_hook(&self, hook: Arc<dyn Fn(String, u32) + Send + Sync>) {
        *self.established_hook.lock().unwrap() = Some(hook);
    }

    /// QSO 已建立时返回 (对方呼号, 对方 uid)，否则 None。
    /// 供 PTT 成员 JSON 发布定向到对端 uid 主题（官方盒子只订阅自己的
    /// uid 主题，发本机主题它们收不到——对齐官方盒子行为必须发对端）。
    pub async fn established_peer(&self) -> Option<(String, u32)> {
        match &*self.phase.lock().await {
            QsoPhase::Established { peer, peer_uid, .. } => Some((peer.clone(), *peer_uid)),
            _ => None,
        }
    }

    pub fn auto_accept(&self) -> bool {
        *self.auto_accept.lock().unwrap()
    }

    pub fn set_auto_accept(&self, enabled: bool) {
        *self.auto_accept.lock().unwrap() = enabled;
        let body = json!({ "autoAccept": enabled });
        if let Ok(text) = serde_json::to_string_pretty(&body) {
            std::fs::write(&self.cfg_path, text).ok();
        }
    }

    // ------------------------------------------------------------ 报文构造/发送

    /// `:TO<补齐9>:<载荷>{<seq>` 的 APFMO0 消息行（载荷可含 GBK 字节）
    fn build_message(&self, to: &str, payload: Vec<u8>, seq: u32) -> Vec<u8> {
        let (my_call, _) = self.identity();
        let mut line = format!("{my_call}>APFMO0,TCPIP*::{:<9.9}", to.to_uppercase()).into_bytes();
        line.push(b':');
        line.extend_from_slice(&payload);
        line.push(b'{');
        line.extend_from_slice(seq.to_string().as_bytes());
        line
    }

    async fn send_to(&self, to: &str, payload: Vec<u8>) -> Result<(), String> {
        let seq = self.next_seq();
        let line = self.build_message(to, payload, seq);
        let preview = String::from_utf8_lossy(&line).into_owned();
        self.tx.send_packet(line).await?;
        (self.emit)(json!({"type": "log", "level": "info", "msg": format!("QSO 发送: {preview}")}));
        Ok(())
    }

    fn log(&self, level: &str, msg: String) {
        (self.emit)(json!({"type": "log", "level": level, "msg": msg}));
    }

    // ------------------------------------------------------------ 状态与事件

    async fn set_phase(&self, phase: QsoPhase, detail: &str) {
        // 进入 Established 是 QSO 祝福发布的单一挂点（先取出，phase 随后被 move 进锁）
        let established = match &phase {
            QsoPhase::Established {
                peer, peer_uid, ..
            } => Some((peer.clone(), *peer_uid)),
            _ => None,
        };
        let (name, peer, peer_uid, outgoing) = match &phase {
            QsoPhase::Idle => ("idle", String::new(), 0u32, false),
            QsoPhase::OutQuery { peer, peer_uid, .. } => {
                ("querying", peer.clone(), *peer_uid, true)
            }
            QsoPhase::OutCall {
                peer,
                peer_uid,
                stage,
                ..
            } => (
                match stage {
                    OutStage::WaitRing => "calling",
                    OutStage::WaitAccept => "ringing",
                },
                peer.clone(),
                *peer_uid,
                true,
            ),
            QsoPhase::InRing { peer, peer_uid, .. } => ("incoming", peer.clone(), *peer_uid, false),
            QsoPhase::Established {
                peer,
                peer_uid,
                outgoing,
                ..
            } => ("established", peer.clone(), *peer_uid, *outgoing),
        };
        *self.phase.lock().await = phase;
        (self.emit)(json!({
            "type": "qso_state", "phase": name, "peer": peer,
            "peerUid": peer_uid, "outgoing": outgoing, "detail": detail,
        }));
        if let Some((peer, peer_uid)) = established {
            if let Some(hook) = self.established_hook.lock().unwrap().clone() {
                hook(peer, peer_uid);
            }
        }
    }

    pub async fn snapshot(&self) -> serde_json::Value {
        let phase = self.phase.lock().await.clone();
        let (name, peer, peer_uid, outgoing) = match &phase {
            QsoPhase::Idle => ("idle", String::new(), 0u32, false),
            QsoPhase::OutQuery { peer, peer_uid, .. } => {
                ("querying", peer.clone(), *peer_uid, true)
            }
            QsoPhase::OutCall {
                peer,
                peer_uid,
                stage,
                ..
            } => (
                match stage {
                    OutStage::WaitRing => "calling",
                    OutStage::WaitAccept => "ringing",
                },
                peer.clone(),
                *peer_uid,
                true,
            ),
            QsoPhase::InRing { peer, peer_uid, .. } => ("incoming", peer.clone(), *peer_uid, false),
            QsoPhase::Established {
                peer,
                peer_uid,
                outgoing,
                ..
            } => ("established", peer.clone(), *peer_uid, *outgoing),
        };
        json!({
            "phase": name, "peer": peer, "peerUid": peer_uid,
            "outgoing": outgoing, "autoAccept": self.auto_accept(),
        })
    }

    // ------------------------------------------------------------ QSO 记录

    fn record(&self, dir: &str, peer: &str, peer_uid: u32, result: &str) {
        let entry = json!({
            "ts": now(),
            "dir": dir,
            "peer": peer,
            "peer_uid": peer_uid,
            "result": result,
        });
        self.push_entry(entry);
    }

    /// 追加一条 qso_log 条目（上限 500 条，溢出丢弃最旧）并通知前端刷新。
    fn push_entry(&self, entry: serde_json::Value) {
        let mut list: Vec<serde_json::Value> = std::fs::read_to_string(&self.log_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        list.push(entry);
        if list.len() > 500 {
            let drop_n = list.len() - 500;
            list.drain(..drop_n);
        }
        if let Ok(text) = serde_json::to_string_pretty(&list) {
            std::fs::write(&self.log_path, text).ok();
        }
        (self.emit)(json!({"type": "qso_log_changed"}));
    }

    /// 收到的完整通联记录（MQTT FMO/QSO/UID/<本机uid>，含 QSO 祝福 toComment）
    /// 写入本地 qso_log —— 固件语义："展示在对方的 QSO 记录中"。
    /// 空祝福（toComment=""）同样写入，不破坏记录。
    pub fn record_remote(&self, rec: &serde_json::Value) {
        let str_field = |key: &str| {
            rec.get(key)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        let ts = rec
            .get("timestamp")
            .and_then(|t| t.as_u64())
            .unwrap_or_else(|| now() as u64);
        let entry = json!({
            "ts": ts,
            "dir": "in",
            "peer": str_field("fromCallsign"),
            "peer_uid": 0,
            "result": "通联记录",
            "comment": str_field("toComment"),
            "grid": str_field("fromGrid"),
            "relay": str_field("relayName"),
        });
        self.push_entry(entry);
    }

    pub fn qso_log(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(&self.log_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    // ------------------------------------------------------------ 出站呼叫（UI 触发）

    pub async fn call(&self, peer: &str, peer_uid: Option<u32>) -> Result<(), String> {
        let peer = peer.trim().to_uppercase();
        if peer.is_empty() {
            return Err("请输入对方呼号".into());
        }
        {
            let phase = self.phase.lock().await;
            if !matches!(&*phase, QsoPhase::Idle) {
                return Err("当前有进行中的 QSO，请先取消/结束".into());
            }
        }
        if base_call(&peer) == base_call(&self.identity().0) {
            return Err("不能呼叫自己".into());
        }
        let uid = match peer_uid {
            Some(u) if u > 0 => u,
            _ => self
                .table
                .lookup_uid_by_callsign(&peer)
                .await
                .ok_or_else(|| format!("不知道 {peer} 的 UID（从用户列表选择或手动输入）"))?,
        };
        self.tx.gate_verified().await?;
        let (my_call, my_uid) = self.identity();
        let _ = my_call;
        let t = now();
        self.send_to(&peer, format!("QTHQRY,Q{my_uid},U{uid}").into_bytes())
            .await?;
        self.set_phase(
            QsoPhase::OutQuery {
                peer: peer.clone(),
                peer_uid: uid,
                started: t,
                last_sent: t,
                deadline: t + QUERY_TIMEOUT_S,
            },
            &format!("正在查询 {peer} 所在服务器…"),
        )
        .await;
        Ok(())
    }

    /// 接听/拒绝来电（弹窗按钮；自动接受时不会用到）
    pub async fn answer(&self, accept: bool) -> Result<(), String> {
        let phase = self.phase.lock().await.clone();
        let QsoPhase::InRing { peer, peer_uid, .. } = phase else {
            return Err("当前没有来电".into());
        };
        if accept {
            self.send_to(&peer, b"CALLANS,ACCEPT".to_vec()).await?;
            self.record("in", &peer, peer_uid, "已接听");
            self.set_phase(
                QsoPhase::Established {
                    peer: peer.clone(),
                    peer_uid,
                    since: now(),
                    outgoing: false,
                },
                &format!("与 {peer} 的 QSO 已建立"),
            )
            .await;
        } else {
            self.send_to(&peer, b"CALLANS,REJECT".to_vec()).await?;
            self.record("in", &peer, peer_uid, "已拒绝");
            self.set_phase(QsoPhase::Idle, &format!("已拒绝 {peer} 的呼叫"))
                .await;
        }
        Ok(())
    }

    /// 取消出站呼叫 / 结束已建立的 QSO
    pub async fn cancel(&self) -> Result<(), String> {
        let phase = self.phase.lock().await.clone();
        match phase {
            QsoPhase::OutQuery { peer, peer_uid, .. }
            | QsoPhase::OutCall { peer, peer_uid, .. } => {
                let (_, my_uid) = self.identity();
                self.send_to(
                    &peer,
                    format!("CALLCANCEL,Q{my_uid},U{peer_uid}").into_bytes(),
                )
                .await
                .ok();
                self.record("out", &peer, peer_uid, "已取消");
                self.set_phase(QsoPhase::Idle, &format!("已取消对 {peer} 的呼叫"))
                    .await;
            }
            QsoPhase::Established {
                peer,
                peer_uid,
                outgoing,
                ..
            } => {
                self.record(
                    if outgoing { "out" } else { "in" },
                    &peer,
                    peer_uid,
                    "已结束",
                );
                self.set_phase(QsoPhase::Idle, &format!("与 {peer} 的 QSO 已结束"))
                    .await;
            }
            QsoPhase::InRing { peer, peer_uid, .. } => {
                self.send_to(&peer, b"CALLANS,REJECT".to_vec()).await.ok();
                self.record("in", &peer, peer_uid, "已拒绝");
                self.set_phase(QsoPhase::Idle, &format!("已拒绝 {peer} 的呼叫"))
                    .await;
            }
            QsoPhase::Idle => {}
        }
        Ok(())
    }

    // ------------------------------------------------------------ 入站信令处理

    pub async fn handle_message(&self, parsed: serde_json::Value) {
        if parsed.get("kind").and_then(|k| k.as_str()) != Some("message") {
            return;
        }
        // 去重：主全馈与上行连接会投递同一条信令（q 构造可能不同，用 来源|目标|动词|msgId 作键）
        let dedup_key = format!(
            "{}|{}|{}|{}",
            parsed
                .get("callsign")
                .and_then(|c| c.as_str())
                .unwrap_or(""),
            parsed.get("to").and_then(|t| t.as_str()).unwrap_or(""),
            parsed.get("verb").and_then(|v| v.as_str()).unwrap_or(""),
            parsed.get("msg_id").and_then(|m| m.as_str()).unwrap_or(""),
        );
        {
            let mut seen = self.seen.lock().unwrap();
            if seen.contains(&dedup_key) {
                return;
            }
            seen.push_back(dedup_key);
            while seen.len() > 200 {
                seen.pop_front();
            }
        }
        let (_, my_uid) = self.identity();
        let my_base = base_call(&self.identity().0);
        let to = parsed.get("to").and_then(|t| t.as_str()).unwrap_or("");
        if base_call(to) != my_base {
            return;
        }
        let from = parsed
            .get("callsign")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let verb = parsed
            .get("verb")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let fields: Vec<String> = parsed
            .get("fields")
            .and_then(|f| f.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        match verb.as_str() {
            "QTHQRY" => self.on_qthqry(&from, &fields, my_uid).await,
            "QTHANS" => self.on_qthans(&from, &fields).await,
            "CALL" => self.on_call(&from, &fields).await,
            "CALLANS" => self.on_callans(&from, &fields).await,
            "CALLCANCEL" => self.on_callcancel(&from).await,
            _ => {}
        }
    }

    /// 字段提取：Q<num> / U<num> / S<num> / LA<ts> / F<num>，剩余第一个当名称
    fn parse_fields(fields: &[String]) -> (Option<u32>, Option<u32>, Option<u32>, String) {
        let (mut q, mut u, mut s, mut name) = (None, None, None, String::new());
        for f in fields {
            let b = f.as_bytes();
            // Q/U/S 标记：首字节 ASCII 字母 + 其余全数字（首字节非 ASCII 的名称不能按字节切，防 panic）
            let matched = if b.len() >= 2 && b[1..].iter().all(|c| c.is_ascii_digit()) {
                match b[0] {
                    b'Q' => {
                        q = f[1..].parse().ok();
                        true
                    }
                    b'U' => {
                        u = f[1..].parse().ok();
                        true
                    }
                    b'S' => {
                        s = f[1..].parse().ok();
                        true
                    }
                    _ => false,
                }
            } else {
                false
            };
            if !matched && !f.starts_with("LA") && !f.starts_with('F') && name.is_empty() {
                name = f.clone();
            }
        }
        (q, u, s, name)
    }

    /// 收到 QTHQRY：自动应答本机当前服务器（固件行为：无需人工）
    async fn on_qthqry(&self, from: &str, fields: &[String], my_uid: u32) {
        let (_, target_uid, _, _) = Self::parse_fields(fields);
        // U 是查询目标 uid：不是查我就不答（呼号带 SSID 时 to 匹配可能误中）
        if let Some(u) = target_uid {
            if my_uid != 0 && u != my_uid {
                return;
            }
        }
        let sel = self.selected_server.lock().await.clone();
        let srv_uid = sel.get("uid").and_then(|u| u.as_u64()).unwrap_or(0) as u32;
        if srv_uid == 0 {
            self.log(
                "warn".into(),
                format!("收到 {from} 的 QTHQRY，但本机未选定服务器，未应答"),
            );
            return;
        }
        let srv_name = sel
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        let la = chrono::Utc::now().format("%Y%m%d%H%M%SZ").to_string();
        let mut payload = format!("QTHANS,F1,U{my_uid},S{srv_uid},LA{la},").into_bytes();
        payload.extend_from_slice(&gbk_bytes(&srv_name));
        if let Err(e) = self.send_to(from, payload).await {
            self.log("warn".into(), format!("应答 {from} 的 QTHQRY 失败：{e}"));
            return;
        }
        self.log(
            "info".into(),
            format!("已应答 {from} 的服务器查询（S{srv_uid} {srv_name}）"),
        );
    }

    /// 收到 QTHANS（我是主叫）：跳到对方服务器并发 CALL
    async fn on_qthans(&self, from: &str, fields: &[String]) {
        let phase = self.phase.lock().await.clone();
        let QsoPhase::OutQuery { peer, peer_uid, .. } = phase else {
            return;
        };
        if base_call(from) != base_call(&peer) {
            return;
        }
        let (_, _, srv_uid, srv_name) = Self::parse_fields(fields);
        let Some(srv_uid) = srv_uid else {
            self.log("warn".into(), format!("{from} 的 QTHANS 缺少服务器编号"));
            return;
        };
        self.log(
            "info".into(),
            format!("{from} 在服务器 S{srv_uid}（{srv_name}），跳台并呼叫…"),
        );
        // 主叫跳到被叫服务器（固件："Remote Has Jumped to Your Server"）
        if let Some(hook) = self.jump_hook.lock().unwrap().clone() {
            hook(srv_uid);
        }
        let (_, my_uid) = self.identity();
        let mut payload = format!("CALL,Q{my_uid},U{peer_uid},S{srv_uid},").into_bytes();
        payload.extend_from_slice(&gbk_bytes(&srv_name));
        if let Err(e) = self.send_to(&peer, payload).await {
            self.set_phase(QsoPhase::Idle, &format!("发送 CALL 失败：{e}"))
                .await;
            return;
        }
        self.set_phase(
            QsoPhase::OutCall {
                peer,
                peer_uid,
                srv_uid,
                srv_name,
                stage: OutStage::WaitRing,
                deadline: now() + RING_TIMEOUT_S,
            },
            "呼叫已发出，等待对方应答…",
        )
        .await;
    }

    /// 收到 CALL（我是被叫）
    async fn on_call(&self, from: &str, fields: &[String]) {
        // Q=主叫 uid（对方），U=被叫 uid（本机）；对方 uid 取 Q 不是 U
        // （取 U 会把 QSO 记录发到自己的 topic）
        let (peer_uid, _, srv_uid, _name) = Self::parse_fields(fields);
        let peer_uid = peer_uid.unwrap_or(0);
        let srv_uid = srv_uid.unwrap_or(0);
        let busy = !matches!(&*self.phase.lock().await, QsoPhase::Idle);
        if busy {
            self.send_to(from, b"CALLANS,BUSY".to_vec()).await.ok();
            self.log("info".into(), format!("忙时收到 {from} 的呼叫，已回 BUSY"));
            return;
        }
        if self.auto_accept() {
            if let Err(e) = self.send_to(from, b"CALLANS,ACCEPT".to_vec()).await {
                self.log("error".into(), format!("自动接受 {from} 失败：{e}"));
                return;
            }
            self.record("in", from, peer_uid, "已接听（自动）");
            self.set_phase(
                QsoPhase::Established {
                    peer: from.to_string(),
                    peer_uid,
                    since: now(),
                    outgoing: false,
                },
                &format!("已自动接受 {from} 的呼叫"),
            )
            .await;
            return;
        }
        self.set_phase(
            QsoPhase::InRing {
                peer: from.to_string(),
                peer_uid,
                srv_uid,
                deadline: now() + IN_RING_TIMEOUT_S,
            },
            &format!("{from} 呼入"),
        )
        .await;
    }

    /// 收到 CALLANS（我是主叫）
    async fn on_callans(&self, from: &str, fields: &[String]) {
        let phase = self.phase.lock().await.clone();
        let QsoPhase::OutCall {
            peer,
            peer_uid,
            srv_uid,
            srv_name,
            stage,
            ..
        } = phase
        else {
            return;
        };
        if base_call(from) != base_call(&peer) {
            return;
        }
        let answer = fields.first().map(|s| s.as_str()).unwrap_or("");
        match answer {
            "RING" => {
                if stage == OutStage::WaitRing {
                    self.set_phase(
                        QsoPhase::OutCall {
                            peer,
                            peer_uid,
                            srv_uid,
                            srv_name,
                            stage: OutStage::WaitAccept,
                            deadline: now() + ACCEPT_TIMEOUT_S,
                        },
                        "对方振铃中…",
                    )
                    .await;
                }
            }
            "ACCEPT" => {
                self.record("out", &peer, peer_uid, "接通");
                self.set_phase(
                    QsoPhase::Established {
                        peer: peer.clone(),
                        peer_uid,
                        since: now(),
                        outgoing: true,
                    },
                    &format!("{peer} 已接听，QSO 建立"),
                )
                .await;
            }
            other => {
                let text = match other {
                    "REJECT" => "对方拒绝",
                    "BUSY" => "对方忙",
                    "DND" => "对方免打扰",
                    "NOTFRIEND" => "对方未加好友",
                    "NOSERVER" => "对方无服务器",
                    "TIMEOUT" => "对方超时",
                    _ => "对方未应答",
                };
                self.record("out", &peer, peer_uid, text);
                self.set_phase(QsoPhase::Idle, &format!("呼叫 {peer} 失败：{text}"))
                    .await;
            }
        }
    }

    /// 收到 CALLCANCEL（对方取消）
    async fn on_callcancel(&self, from: &str) {
        let phase = self.phase.lock().await.clone();
        match phase {
            QsoPhase::InRing { peer, peer_uid, .. } if base_call(&peer) == base_call(from) => {
                self.record("in", &peer, peer_uid, "对方取消");
                self.set_phase(QsoPhase::Idle, &format!("{peer} 取消了呼叫"))
                    .await;
            }
            QsoPhase::Established {
                peer,
                peer_uid,
                outgoing,
                ..
            } if base_call(&peer) == base_call(from) => {
                self.record(
                    if outgoing { "out" } else { "in" },
                    &peer,
                    peer_uid,
                    "对方结束",
                );
                self.set_phase(QsoPhase::Idle, &format!("{peer} 结束了 QSO"))
                    .await;
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ 超时 tick（1s）

    pub async fn tick(&self) {
        let phase = self.phase.lock().await.clone();
        let t = now();
        match phase {
            QsoPhase::OutQuery {
                peer,
                peer_uid,
                last_sent,
                deadline,
                ..
            } => {
                if t >= deadline {
                    self.record("out", &peer, peer_uid, "查询无应答");
                    self.set_phase(QsoPhase::Idle, &format!("{peer} 未应答服务器查询"))
                        .await;
                } else if t - last_sent >= QUERY_RETRY_S {
                    // 重发 QTHQRY（msgId 递增，与原厂固件一致）
                    let (_, my_uid) = self.identity();
                    if self
                        .send_to(&peer, format!("QTHQRY,Q{my_uid},U{peer_uid}").into_bytes())
                        .await
                        .is_ok()
                    {
                        if let QsoPhase::OutQuery { last_sent: ls, .. } =
                            &mut *self.phase.lock().await
                        {
                            *ls = t;
                        }
                    }
                }
            }
            QsoPhase::OutCall {
                peer,
                peer_uid,
                stage,
                deadline,
                ..
            } => {
                if t >= deadline {
                    let text = match stage {
                        OutStage::WaitRing => "对方无应答",
                        OutStage::WaitAccept => "对方未接听（超时）",
                    };
                    self.record("out", &peer, peer_uid, text);
                    self.set_phase(QsoPhase::Idle, &format!("呼叫 {peer} 失败：{text}"))
                        .await;
                }
            }
            QsoPhase::InRing {
                peer,
                peer_uid,
                deadline,
                ..
            } => {
                if t >= deadline {
                    self.record("in", &peer, peer_uid, "未接来电");
                    self.set_phase(QsoPhase::Idle, &format!("未接来电：{peer}"))
                        .await;
                }
            }
            _ => {}
        }
    }

    pub fn start_tick_task(self: &Arc<Self>) {
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                this.tick().await;
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_qthqry_fields() {
        let fields = vec!["Q3187".to_string(), "U2533".to_string()];
        let (q, u, s, name) = QsoEngine::parse_fields(&fields);
        assert_eq!(q, Some(3187));
        assert_eq!(u, Some(2533));
        assert_eq!(s, None);
        assert_eq!(name, "");
    }

    #[test]
    fn parse_qthans_fields() {
        // 实捕：QTHANS,F1,U2725,S2579,LA20260806010157Z,<GBK服务器名>
        let fields = vec![
            "F1".to_string(),
            "U2725".to_string(),
            "S2579".to_string(),
            "LA20260806010157Z".to_string(),
            "河北某地".to_string(),
        ];
        let (q, u, s, name) = QsoEngine::parse_fields(&fields);
        assert_eq!(q, None);
        assert_eq!(u, Some(2725));
        assert_eq!(s, Some(2579));
        assert_eq!(name, "河北某地");
    }

    #[test]
    fn parse_call_fields() {
        let fields = vec![
            "Q796".to_string(),
            "U2533".to_string(),
            "S2579".to_string(),
            "测试台".to_string(),
        ];
        let (q, u, s, name) = QsoEngine::parse_fields(&fields);
        assert_eq!(q, Some(796));
        assert_eq!(u, Some(2533));
        assert_eq!(s, Some(2579));
        assert_eq!(name, "测试台");
    }

    #[test]
    fn base_call_strips_ssid() {
        assert_eq!(base_call("bd4xgt-15"), "BD4XGT");
        assert_eq!(base_call("BG9JYT"), "BG9JYT");
    }

    #[test]
    fn maidenhead_grid_known_points() {
        // 标准 6 位换算（与实捕样本 OM89av 同格式：字段大写/数字/子方小写）
        assert_eq!(maidenhead_grid(39.9, 116.4), "OM89ev");
        assert_eq!(maidenhead_grid(32.3932, 119.3706), "OM92qj");
        // 南半球/西经（手算向量：-33.865,-74.006 → FF26xd）
        assert_eq!(maidenhead_grid(-33.865, -74.006), "FF26xd");
        // 非法输入返回空串
        assert_eq!(maidenhead_grid(f64::NAN, 116.4), "");
    }

    #[test]
    fn grid_to_latlon_roundtrip() {
        // 与 maidenhead_grid 互逆：编码后的网格解码回方格中心，中心再编码应得同网格
        for (la, lo, g) in [
            (39.9, 116.4, "OM89ev"),
            (32.3932, 119.3706, "OM92qj"),
            (-33.865, -74.006, "FF26xd"),
        ] {
            let (dla, dlo) = grid_to_latlon(g).expect("合法网格应可解码");
            assert_eq!(maidenhead_grid(dla, dlo), g, "方格中心再编码应还原 {g}");
            // 中心点与原点的偏差应在半个子方内（lon ≤2.5′, lat ≤1.25′）
            assert!((dla - la).abs() <= 1.25 / 60.0 + 1e-9);
            assert!((dlo - lo).abs() <= 2.5 / 60.0 + 1e-9);
        }
        // 4 位网格：方格 2°×1° 中心
        assert_eq!(grid_to_latlon("OM89"), Some((39.5, 117.0)));
        // 小写/空白容错
        assert_eq!(grid_to_latlon(" om89ev "), grid_to_latlon("OM89ev"));
        // 非法输入
        assert!(grid_to_latlon("").is_none());
        assert!(grid_to_latlon("OM89e").is_none());
        assert!(grid_to_latlon("ZZ89ev").is_none());
        assert!(grid_to_latlon("OM89evx").is_none());
    }

    #[test]
    fn geo_distance_bearing_known() {
        // 北京(39.9,116.4) → 上海(31.2,121.5)：约 1067km，方位约 153°（南偏东）
        let (d, b) = geo_distance_bearing(39.9, 116.4, 31.2, 121.5);
        assert!((d / 1000.0 - 1067.0).abs() < 20.0, "距离 {d} 应约 1067km");
        assert!((b - 153.0).abs() < 5.0, "方位 {b} 应约 153°");
        // 正北/正东/同点边界
        let (_, bn) = geo_distance_bearing(39.9, 116.4, 49.9, 116.4);
        assert!(bn < 0.5 || bn > 359.5, "正北方位 {bn}");
        let (_, be) = geo_distance_bearing(0.0, 0.0, 0.0, 1.0);
        assert!((be - 90.0).abs() < 0.5, "赤道正东方位 {be}");
        let (d0, _) = geo_distance_bearing(39.9, 116.4, 39.9, 116.4);
        assert!(d0 < 1.0, "同点距离 {d0}");
    }

    #[test]
    fn compass16_sectors() {
        assert_eq!(compass16(0.0), "N");
        assert_eq!(compass16(11.24), "N");
        assert_eq!(compass16(11.26), "NNE");
        assert_eq!(compass16(90.0), "E");
        assert_eq!(compass16(180.0), "S");
        assert_eq!(compass16(270.0), "W");
        assert_eq!(compass16(359.9), "N");
        assert_eq!(compass16(348.76), "N");
        assert_eq!(compass16(337.5), "NNW");
        assert_eq!(compass16(326.26), "NNW");
    }

    #[test]
    fn qso_record_json_layout() {
        // 固件模板字段全集；空祝福不破坏记录
        let rec = build_qso_record(
            7,
            1786318787,
            438_500_000,
            "BD4XGT",
            "OM89ev",
            "BG8LLD",
            "",
            "73 通联愉快",
            "FMO",
            "测试台",
            "BG9JYT",
        );
        assert_eq!(rec["logId"], 7);
        assert_eq!(rec["timestamp"], 1786318787u64);
        assert_eq!(rec["freqHz"], 438_500_000u64);
        assert_eq!(rec["fromCallsign"], "BD4XGT");
        assert_eq!(rec["fromGrid"], "OM89ev");
        assert_eq!(rec["toCallsign"], "BG8LLD");
        assert_eq!(rec["toComment"], "73 通联愉快");
        assert_eq!(rec["mode"], "FMO");
        assert_eq!(rec["relayName"], "测试台");
        assert_eq!(rec["relayAdmin"], "BG9JYT");
        let empty = build_qso_record(1, 0, 0, "A", "", "B", "", "", "FMO", "", "");
        assert_eq!(empty["toComment"], "");
        assert_eq!(empty["freqHz"], 0);
        // 可序列化为单行 JSON（MQTT 载荷）
        let text = serde_json::to_string(&rec).unwrap();
        assert!(text.contains("\"toComment\":\"73 通联愉快\""));
    }
}
