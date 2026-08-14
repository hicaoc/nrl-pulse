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
    cfg_path: PathBuf,
    log_path: PathBuf,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// 去掉 SSID 的大写基呼号
fn base_call(cs: &str) -> String {
    cs.split('-').next().unwrap_or(cs).trim().to_uppercase()
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
        if *self.tx.state.lock().await != "verified" {
            return Err("APRS 上行未验证登录（先连接 APRS 且 passcode 正确）".into());
        }
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
        let (_, peer_uid, srv_uid, _name) = Self::parse_fields(fields);
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
}
