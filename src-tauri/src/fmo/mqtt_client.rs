//! rumqttc FMO 连接骨架。

use rand::RngCore;
use rumqttc::v5::mqttbytes::v5::{ConnectReturnCode, Filter, Packet};
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::{AsyncClient, Event, MqttOptions};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::fmo::aprs::EmitFn;

pub const SUBSCRIBE_TOPICS: &[&str] = &[
    "FMO/RAW",
    "FMO/TELE",
    "FMO/SERVER_INFO",
    "FMO/PROFILE",
    "FMO/LATE/UID_V1/",
    "FMO/QSO/UID/#",
];

fn subscription_filters(no_local: bool) -> Vec<Filter> {
    SUBSCRIBE_TOPICS
        .iter()
        .map(|topic| {
            let mut filter = Filter::new((*topic).to_string(), QoS::AtMostOnce);
            filter.nolocal = no_local;
            filter
        })
        .collect()
}

pub fn client_id_for(callsign: &str, uid: u32, suffix: &str) -> String {
    let cs = callsign.to_uppercase();
    let cs = if cs.is_empty() {
        "N0CALL".to_string()
    } else {
        cs
    };
    format!("FMO-{cs}-{uid}-{suffix}")
}

/// Generate one random identifier suffix for this application process. It is
/// reused by reconnects, but changes after the application restarts so cloned
/// installations using the same FMO identity do not continuously kick each
/// other off the broker.
fn new_client_suffix() -> String {
    let mut bytes = [0u8; 2];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode_upper(bytes)
}

/// GBK/UTF-8 容错解码。
fn decode_text(b: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(b) {
        return s.trim_end_matches('\x00').to_string();
    }
    let (decoded, _enc, _err) = encoding_rs::GBK.decode(b);
    decoded.trim_end_matches('\x00').to_string()
}

fn readable_msg(topic: &str, payload: &[u8]) -> String {
    match topic {
        "FMO/RAW" => format!("RAW {}B", payload.len()),
        "FMO/TELE" => parse_tele(payload),
        "FMO/SERVER_INFO" => parse_server_info(payload),
        "FMO/PROFILE" => parse_profile(payload),
        _ if topic.starts_with("FMO/QSO") => parse_qso(payload),
        _ => {
            let head = &payload[..payload.len().min(40)];
            let text = String::from_utf8_lossy(head);
            if text.contains('\u{FFFD}') {
                format!("{topic} {}B {:?}", payload.len(), hex::encode(head))
            } else {
                format!("{topic}: {}", text.trim_end_matches('\x00'))
            }
        }
    }
}

/// 定长零填充字段 → str（参照 open-fmo sim 的 _cstr）。
fn cstr_field(b: &[u8], off: usize, maxlen: usize) -> String {
    let end = b
        .get(off..off + maxlen)
        .and_then(|s| s.iter().position(|&x| x == 0))
        .map(|i| off + i)
        .unwrap_or_else(|| (off + maxlen).min(b.len()));
    decode_text(&b[off..end.min(b.len())])
}

/// Unix 秒 → 人类可读（本地时间），≤0 显示 —。
fn fmt_ts(ts: i64) -> String {
    if ts <= 0 {
        return "—".to_string();
    }
    let dt = chrono::DateTime::from_timestamp(ts, 0).unwrap_or_default();
    dt.format("%m-%d %H:%M:%S").to_string()
}

/// 从二进制里提取可读字符串（ASCII），用于未完全逆向字段的兜底展示。
fn extract_strings(b: &[u8], from: usize) -> Vec<String> {
    let mut out = Vec::new();
    let data = if from < b.len() { &b[from..] } else { &[] };
    let mut cur: Vec<u8> = Vec::new();
    for &byte in data {
        if (0x20..=0x7e).contains(&byte) {
            cur.push(byte);
        } else {
            if cur.len() >= 4 {
                out.push(String::from_utf8_lossy(&cur).into_owned());
            }
            cur.clear();
        }
    }
    if cur.len() >= 4 {
        out.push(String::from_utf8_lossy(&cur).into_owned());
    }
    // 去重保序，最多 6 条
    let mut seen = std::collections::HashSet::new();
    out.into_iter()
        .filter(|s| seen.insert(s.clone()))
        .take(6)
        .collect()
}

/// 解析 FMO/TELE 遥测帧（33B，参照 open-fmo sim 逆向结论）：
/// [0]0x02 [1:5]u32 设备ID [5:9]u32 计数器 [9:21]呼号12B
/// [21:25]u32 Unix时间戳（可为0） [25:29]f32频率1 [29:33]f32频率2
pub fn parse_tele(payload: &[u8]) -> String {
    if payload.len() < 33 || payload[0] != 0x02 {
        return format!("{}B {}", payload.len(), hex::encode(payload));
    }
    let dev_id = u32::from_le_bytes(payload[1..5].try_into().unwrap_or([0; 4]));
    let counter = u32::from_le_bytes(payload[5..9].try_into().unwrap_or([0; 4]));
    let callsign = cstr_field(payload, 9, 12);
    let ts = u32::from_le_bytes(payload[21..25].try_into().unwrap_or([0; 4]));
    let f1 = f32::from_le_bytes(payload[25..29].try_into().unwrap_or([0; 4]));
    let f2 = f32::from_le_bytes(payload[29..33].try_into().unwrap_or([0; 4]));
    let freq = if (f1 - f2).abs() < 0.001 {
        format!("{f1:.3}")
    } else {
        format!("{f1:.3}/{f2:.3}")
    };
    format!(
        "台站 {callsign} 频率 {freq}MHz 时间 {} id={dev_id:08x} 计数{counter}",
        fmt_ts(ts as i64)
    )
}

/// 解析 FMO/SERVER_INFO（569B）：
/// [0]0x01 [1:5]u32序号 [5:9]u32常量 [9:21]呼号12B [25:55]名称30B [55:85]简介30B
pub fn parse_server_info(payload: &[u8]) -> String {
    if payload.len() < 25 {
        return format!("{}B {}", payload.len(), hex::encode(payload));
    }
    let seq = u32::from_le_bytes(payload[1..5].try_into().unwrap_or([0; 4]));
    let callsign = cstr_field(payload, 9, 12);
    let name = if payload.len() > 25 {
        cstr_field(payload, 25, 30)
    } else {
        String::new()
    };
    let desc = if payload.len() > 55 {
        cstr_field(payload, 55, 30)
    } else {
        String::new()
    };
    let mut out = format!("序号{seq} 服务器 {callsign}");
    if !name.is_empty() {
        out.push_str(&format!(" 名称[{name}]"));
    }
    if !desc.is_empty() {
        out.push_str(&format!(" 简介[{desc}]"));
    }
    // 兜底：从剩余字节提取可读字符串
    let extra = extract_strings(payload, 85);
    if !extra.is_empty() {
        out.push_str(&format!(" 其他: {}", extra.join(" | ")));
    }
    out
}

/// 解析 FMO/PROFILE（128B）：id + 若干 u32 + 可读字符串兜底。
pub fn parse_profile(payload: &[u8]) -> String {
    if payload.len() < 32 {
        return format!(
            "{}B {}",
            payload.len(),
            hex::encode(&payload[..payload.len().min(64)])
        );
    }
    let id = u32::from_le_bytes(payload[1..5].try_into().unwrap_or([0; 4]));
    let mut u32s = Vec::new();
    for off in [8usize, 12, 16] {
        if off + 4 <= payload.len() {
            u32s.push(u32::from_le_bytes(
                payload[off..off + 4].try_into().unwrap_or([0; 4]),
            ));
        }
    }
    let strings = extract_strings(payload, 0);
    let mut out = format!("id={id:08x} u32={u32s:?}");
    if !strings.is_empty() {
        out.push_str(&format!(" 字符串: {}", strings.join(" | ")));
    }
    out
}

/// 解析 FMO/QSO/UID/{uid} 通联记录（264B）：
/// [0:4]u32 类型(1) [12:16]u32 uid [16:20]u32 Unix时间戳
/// [24:36]对方呼号12B [40:52]Maidenhead网格12B [48:60]中继/服务器名UTF-8
pub fn parse_qso(payload: &[u8]) -> String {
    if payload.len() < 60 {
        return format!(
            "{}B {}",
            payload.len(),
            hex::encode(&payload[..payload.len().min(64)])
        );
    }
    let typ = u32::from_le_bytes(payload[0..4].try_into().unwrap_or([0; 4]));
    let uid = u32::from_le_bytes(payload[12..16].try_into().unwrap_or([0; 4]));
    let ts = u32::from_le_bytes(payload[16..20].try_into().unwrap_or([0; 4]));
    let callsign = cstr_field(payload, 24, 12);
    let grid = cstr_field(payload, 40, 12);
    let name = cstr_field(payload, 48, 12);
    let mut out = format!("通联 uid={uid} 对方 {callsign}");
    if !grid.is_empty() {
        out.push_str(&format!(" 网格[{grid}]"));
    }
    if !name.is_empty() {
        out.push_str(&format!(" 中继[{name}]"));
    }
    out.push_str(&format!(" 时间 {} 类型{typ}", fmt_ts(ts as i64)));
    out
}

// ---------------------------------------------------------------- FMO MQTT 客户端

/// 单个服务器的收包统计。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ServerTraffic {
    pub count: usize,
    pub raw_frames: usize,
    pub tele: usize,
    pub server_info: usize,
    pub profile: usize,
    pub last_topic: String,
    pub last_msg: String,
    pub last_ts: u64,
}

pub struct FmoMqttClient {
    pub emit: EmitFn,
    pub on_raw_payload: std::sync::Mutex<Option<Arc<dyn Fn(Vec<u8>) + Send + Sync>>>,
    /// 凭据工厂：role → (username, password)。
    /// SAS ACL 要求 claimed role 与证书登记角色一致（如登记 super 却声称 user 会被拒），
    /// 认证被拒时按 ROLE_SEQ 换角色重建凭据重试（参照 sim 的 _role_seq 机制）。
    pub cred_factory: std::sync::Mutex<
        Option<Arc<dyn Fn(&str) -> Result<(String, String), String> + Send + Sync>>,
    >,
    pub state: Arc<Mutex<String>>,
    pub detail: Arc<Mutex<String>>,
    pub client: Arc<Mutex<Option<AsyncClient>>>,
    pub generation: Arc<std::sync::atomic::AtomicU64>,
    pub current_host: Arc<Mutex<String>>,
    /// Random for this process and stable across reconnects.
    pub client_suffix: String,
    /// Full client ID of the current or most recent MQTT session.
    pub current_client_id: Arc<Mutex<String>>,
    /// MQTT 5 No Local subscription option; enabled by default.
    pub no_local: Arc<std::sync::atomic::AtomicBool>,
    pub traffic: Arc<Mutex<std::collections::BTreeMap<String, ServerTraffic>>>,
    /// FMO 顶栏全局计数：遥测（TELE+SERVER_INFO 消息数）/ 文本（RAW 以外的其它消息数）
    pub cnt_tele: Arc<std::sync::atomic::AtomicU64>,
    pub cnt_text: Arc<std::sync::atomic::AtomicU64>,
}

/// 认证被拒时按序重试的角色（与 sim-rust / sim 一致）
const ROLE_SEQ: [&str; 3] = ["user", "super", "admin"];

impl FmoMqttClient {
    pub fn new(emit: EmitFn) -> Self {
        Self {
            emit,
            on_raw_payload: std::sync::Mutex::new(None),
            cred_factory: std::sync::Mutex::new(None),
            state: Arc::new(Mutex::new("disconnected".into())),
            detail: Arc::new(Mutex::new(String::new())),
            client: Arc::new(Mutex::new(None)),
            generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            current_host: Arc::new(Mutex::new(String::new())),
            client_suffix: new_client_suffix(),
            current_client_id: Arc::new(Mutex::new(String::new())),
            no_local: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            traffic: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            cnt_tele: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            cnt_text: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    async fn set_state(&self, state: &str, detail: &str) {
        *self.state.lock().await = state.to_string();
        *self.detail.lock().await = detail.to_string();
        let client_id = self.current_client_id.lock().await.clone();
        (self.emit)(
            json!({"type": "mqtt_state", "state": state, "detail": detail,
                           "client_id": client_id}),
        );
    }

    pub async fn connect(
        &self,
        host: &str,
        port: u16,
        uid: u32,
        username: Option<String>,
        password: Option<String>,
        tls: bool,
        callsign: Option<String>,
        initial_role: String,
    ) {
        let mut tls = tls;
        let port = port;
        let host = host.to_string();
        if tls && port == 1883 {
            tls = false;
            (self.emit)(json!({"type": "log", "level": "warn",
                "msg": "1883 为明文端口，已自动关闭 TLS（TLS 请用 8883）"}));
        } else if port == 8883 && !tls {
            tls = true;
        }
        self.disconnect().await;
        let cs = callsign.unwrap_or_default();
        let cid = client_id_for(&cs, uid, &self.client_suffix);
        *self.current_client_id.lock().await = cid.clone();
        // 连接前预检：解析 host 并测试 TCP 连通，失败快速返回明确错误
        {
            let addr_str = format!("{host}:{port}");
            // 传 owned String 避免借用冲突
            let lookup = tokio::net::lookup_host(addr_str.clone()).await;
            match lookup {
                Ok(mut addrs) => {
                    let mut connected = false;
                    let mut first_err: Option<String> = None;
                    while let Some(addr) = addrs.next() {
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            tokio::net::TcpStream::connect(addr),
                        )
                        .await
                        {
                            Ok(Ok(_)) => {
                                connected = true;
                                break;
                            }
                            Ok(Err(e)) => {
                                if first_err.is_none() {
                                    first_err = Some(e.to_string());
                                }
                            }
                            Err(_) => {
                                if first_err.is_none() {
                                    first_err = Some("connect timed out".into());
                                }
                            }
                        }
                    }
                    if !connected {
                        let first_err = first_err.unwrap_or_else(|| "no address".into());
                        let msg = format!(
                            "无法连接 MQTT 服务器 {addr_str}（{first_err}）。请检查服务器地址/端口是否正确"
                        );
                        self.set_state("error", &msg).await;
                        (self.emit)(json!({"type": "log", "level": "error", "msg": msg}));
                        return;
                    }
                }
                Err(e) => {
                    let msg = format!("MQTT 服务器 DNS 解析失败 {addr_str}: {e}");
                    self.set_state("error", &msg).await;
                    (self.emit)(json!({"type": "log", "level": "error", "msg": msg}));
                    return;
                }
            }
        }
        let mut opts = MqttOptions::new(cid.clone(), host.to_string(), port);
        opts.set_keep_alive(std::time::Duration::from_secs(60));
        if let Some(u) = username {
            opts.set_credentials(u, password.unwrap_or_default());
        }
        if tls {
            opts.set_transport(rumqttc::Transport::tls_with_default_config());
        }
        let (client, mut eventloop) = AsyncClient::new(opts, 100);
        let gen = self
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;

        let emit = self.emit.clone();
        let state = self.state.clone();
        let detail = self.detail.clone();
        let on_raw = self.on_raw_payload.lock().unwrap().clone();
        let cred_factory = self.cred_factory.lock().unwrap().clone();
        let client_holder = self.client.clone();
        let generation = self.generation.clone();
        let traffic = self.traffic.clone();
        let cnt_tele = self.cnt_tele.clone();
        let cnt_text = self.cnt_text.clone();
        let no_local = self.no_local.clone();
        *self.current_host.lock().await = host.clone();

        *self.client.lock().await = Some(client.clone());
        self.set_state("connecting", &format!("{host}:{port}"))
            .await;

        tauri::async_runtime::spawn(async move {
            let mut client = client;
            let mut subscribed = false;
            // 初始角色由调用方按「服务器呼号 == 证书呼号 → super，否则 user」选定；
            // 被拒后从该角色起按 ROLE_SEQ 往后重试
            let mut role_idx = ROLE_SEQ
                .iter()
                .position(|r| *r == initial_role)
                .unwrap_or(0);
            loop {
                if generation.load(std::sync::atomic::Ordering::SeqCst) != gen {
                    return;
                }
                match eventloop.poll().await {
                    Ok(Event::Incoming(Packet::ConnAck(ack))) => {
                        let code = ack.code;
                        eprintln!("[FMO-MQTT] ConnAck code={code:?} host={host}:{port}");
                        if code != ConnectReturnCode::Success {
                            // 认证被拒：SAS 可能要求 claimed role 与证书登记角色一致，
                            // 按 ROLE_SEQ（user→super→admin）换角色重建凭据重试
                            if matches!(code, ConnectReturnCode::BadUserNamePassword | ConnectReturnCode::NotAuthorized) && role_idx + 1 < ROLE_SEQ.len() {
                                let cur = ROLE_SEQ[role_idx];
                                let next = ROLE_SEQ[role_idx + 1];
                                let retry = match &cred_factory {
                                    Some(factory) => factory(next).ok(),
                                    None => None,
                                };
                                if let Some((u, p)) = retry {
                                    (emit)(json!({"type": "log", "level": "warn",
                                        "msg": format!("MQTT 角色 {cur} 被拒（code={code:?}），换 {next} 重试…")}));
                                    role_idx += 1;
                                    let mut opts2 =
                                        MqttOptions::new(cid.clone(), host.clone(), port);
                                    opts2.set_keep_alive(std::time::Duration::from_secs(60));
                                    opts2.set_credentials(u, p);
                                    if tls {
                                        opts2.set_transport(
                                            rumqttc::Transport::tls_with_default_config(),
                                        );
                                    }
                                    let (new_client, new_eventloop) = AsyncClient::new(opts2, 100);
                                    *client_holder.lock().await = Some(new_client.clone());
                                    client = new_client;
                                    eventloop = new_eventloop;
                                    subscribed = false;
                                    continue;
                                }
                            }
                            let reason = match code {
                                ConnectReturnCode::BadUserNamePassword => "用户/密码错误或认证被拒",
                                ConnectReturnCode::NotAuthorized => "未授权",
                                ConnectReturnCode::ProtocolError => "协议错误",
                                ConnectReturnCode::ClientIdentifierNotValid => "客户端 ID 无效",
                                _ => "服务器拒绝连接",
                            };
                            let msg = format!(
                                "MQTT 连接被服务器拒绝（code={code} {reason}）。\n\
                                 host={host} port={port}\n\
                                 请检查证书(cert_user/cert_int/cert_devicekey)是否正确、\
                                 呼号/uid 是否与证书一致、服务器是否允许当前身份连接",
                                code = format!("{code:?}")
                            );
                            *state.lock().await = "error".to_string();
                            *detail.lock().await = msg.clone();
                            (emit)(json!({"type": "log", "level": "error", "msg": msg}));
                            (emit)(
                                json!({"type": "mqtt_state", "state": "error", "detail": msg,
                                         "client_id": cid}),
                            );
                            return;
                        }
                        *state.lock().await = "connected".to_string();
                        (emit)(json!({"type": "mqtt_state", "state": "connected",
                                      "detail": format!("{host}:{port}"), "client_id": cid}));
                        (emit)(json!({"type": "log", "level": "info",
                        "msg": format!("MQTT 已连接 {host}:{port}，client id={cid}，订阅 {} 个 topic{}",
                                       SUBSCRIBE_TOPICS.len(),
                                       if role_idx > 0 {
                                           format!("（角色 {}）", ROLE_SEQ[role_idx])
                                       } else {
                                           String::new()
                                       })}));
                        if !subscribed {
                            let filters = subscription_filters(
                                no_local.load(std::sync::atomic::Ordering::Relaxed),
                            );
                            let _ = client.subscribe_many(filters).await;
                            subscribed = true;
                        }
                    }
                    Ok(Event::Incoming(Packet::Publish(pub_msg))) => {
                        let topic = String::from_utf8_lossy(&pub_msg.topic).into_owned();
                        let payload = pub_msg.payload.to_vec();
                        {
                            let mut t = traffic.lock().await;
                            let st = t.entry(host.clone()).or_default();
                            st.count += 1;
                            if topic == "FMO/RAW" {
                                st.raw_frames += 1;
                            } else if topic == "FMO/TELE" {
                                st.tele += 1;
                            } else if topic == "FMO/SERVER_INFO" {
                                st.server_info += 1;
                            } else if topic == "FMO/PROFILE" {
                                st.profile += 1;
                            }
                            st.last_topic = topic.clone();
                            st.last_msg = readable_msg(&topic, &payload);
                            st.last_ts = chrono::Utc::now().timestamp() as u64;
                            // FMO 顶栏全局计数：遥测 = TELE+SERVER_INFO；文本 = RAW 以外其它消息
                            if topic == "FMO/TELE" || topic == "FMO/SERVER_INFO" {
                                cnt_tele.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            } else if topic != "FMO/RAW" {
                                cnt_text.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            let snapshot = st.clone();
                            (emit)(json!({
                                "type": "server_traffic", "host": host, "traffic": snapshot,
                            }));
                        }
                        if topic == "FMO/RAW" {
                            if let Some(cb) = &on_raw {
                                let _ = cb(payload);
                            }
                        } else if topic == "FMO/TELE" {
                            (emit)(json!({"type": "log", "level": "info",
                                "msg": format!("MQTT FMO/TELE: {}", parse_tele(&payload))}));
                        } else if topic == "FMO/SERVER_INFO" {
                            (emit)(json!({"type": "log", "level": "info",
                                "msg": format!("MQTT FMO/SERVER_INFO: {}", parse_server_info(&payload))}));
                        } else {
                            // 其余 topic（PROFILE/QSO/LATE 等）同样解析为可读文本，不再丢 hex
                            (emit)(json!({"type": "log", "level": "info",
                                "msg": format!("MQTT {}", readable_msg(&topic, &payload))}));
                        }
                    }
                    Ok(Event::Incoming(Packet::Disconnect(_))) => {
                        *state.lock().await = "disconnected".to_string();
                        (emit)(json!({"type": "mqtt_state", "state": "disconnected",
                                      "detail": "对端断开", "client_id": cid}));
                    }
                    Ok(Event::Incoming(_)) => {}
                    Ok(Event::Outgoing(_)) => {}
                    Err(e) => {
                        if generation.load(std::sync::atomic::Ordering::SeqCst) != gen {
                            return;
                        }
                        let mut msg = format!("MQTT 连接失败：{e}");
                        // 网络超时：TCP 或 CONNECT 握手未完成，多半是认证/端口/服务器问题
                        if e.to_string().contains("timeout")
                            || e.to_string().contains("Timeout")
                            || e.to_string().contains("timed out")
                        {
                            msg = format!(
                                "MQTT 连接超时：无法在 {}/{} 完成握手。请检查：\n\
                                 1) 服务器地址/端口是否正确（STATION 广播的 host:port）\n\
                                 2) 是否需要 TLS（8883）\n\
                                 3) 证书（cert_user/cert_int/cert_devicekey）是否与服务器匹配\n\
                                 4) 服务器是否在线",
                                host, port
                            );
                        }
                        *state.lock().await = "error".to_string();
                        *detail.lock().await = msg.clone();
                        (emit)(json!({"type": "log", "level": "error", "msg": msg}));
                        (emit)(
                            json!({"type": "mqtt_state", "state": "error", "detail": msg,
                                     "client_id": cid}),
                        );
                        // 超时/认证失败不做无限重连，避免反复等待；用户可手动重新连接
                        return;
                    }
                }
            }
        });
    }

    pub async fn disconnect(&self) {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *self.client.lock().await = None;
        self.set_state("disconnected", "用户断开").await;
    }

    pub async fn set_no_local(&self, enabled: bool) -> Result<(), String> {
        self.no_local
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
        if let Some(client) = self.client.lock().await.clone() {
            client
                .subscribe_many(subscription_filters(enabled))
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub fn no_local_enabled(&self) -> bool {
        self.no_local.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub async fn publish(&self, topic: &str, payload: Vec<u8>, qos: u8) -> Result<(), String> {
        let client = self.client.lock().await.clone();
        let Some(client) = client else {
            return Err("MQTT 未连接".into());
        };
        let q = if qos >= 2 {
            QoS::ExactlyOnce
        } else if qos == 1 {
            QoS::AtLeastOnce
        } else {
            QoS::AtMostOnce
        };
        client
            .publish(topic, q, false, payload)
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn state_str(&self) -> String {
        self.state.lock().await.clone()
    }

    pub async fn client_id_str(&self) -> String {
        self.current_client_id.lock().await.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mqtt_client_id_uses_callsign_uid_and_random_suffix_shape() {
        let suffix = new_client_suffix();
        assert_eq!(suffix.len(), 4);
        assert!(suffix
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b)));
        assert_eq!(
            client_id_for("bg8lld", 42, &suffix),
            format!("FMO-BG8LLD-42-{suffix}")
        );
    }

    #[test]
    fn tele_parse() {
        let s = "02568d257cd20800004247384c4c440000000000001363786acd0cd743cd0cd743";
        let out = parse_tele(&hex::decode(s).unwrap());
        assert!(out.contains("BG8LLD"), "应含呼号: {out}");
        assert!(out.contains("430.100"), "应含频率: {out}");
        assert!(out.contains("id=7c258d56"), "应含设备ID: {out}");
        assert!(out.contains("计数2258"), "应含计数器: {out}");
        assert!(out.contains("时间 08-"), "应含人类可读时间: {out}");
    }

    #[test]
    fn tele_parse_heterodyne() {
        // 异频中继：BD8CCO 频率1=438.26 频率2=439.18，应显示 438.260/439.180
        let mut b = vec![0x02];
        b.extend_from_slice(&0x12345678u32.to_le_bytes());
        b.extend_from_slice(&7u32.to_le_bytes());
        b.extend_from_slice(b"BD8CCO\x00\x00\x00\x00\x00\x00");
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&438.26f32.to_le_bytes());
        b.extend_from_slice(&439.18f32.to_le_bytes());
        assert_eq!(b.len(), 33);
        let out = parse_tele(&b);
        assert!(out.contains("438.260/439.180"), "异频应显示两个频率: {out}");
        assert!(out.contains("BD8CCO"), "应含呼号: {out}");
    }

    #[test]
    fn server_info_parse() {
        let s = "01210000005e00000042413841454400000000000000000000e8a5bfe58d97e99b86e7bea428e982b5e5ae8129000000000000000000000000e6aca2e8bf8ee6";
        let out = parse_server_info(&hex::decode(s).unwrap());
        assert!(out.contains("BA8AED"), "应含呼号: {out}");
        assert!(out.contains("序号33"), "应含序号: {out}");
        assert!(out.contains("西南集群"), "应含中文名: {out}");
    }

    #[test]
    fn server_info_parse_zero_flag() {
        // [0]=0x00 的样本（如意甘肃 BG9JYT），呼号 12B 从 [9:21]
        let s = "00110100003c0100004247394a595400000000000000000000e5a682e6848fe79498e882830000000000000000000000000000000000000000e6a087e58786e7";
        let out = parse_server_info(&hex::decode(s).unwrap());
        assert!(out.contains("BG9JYT"), "应含呼号: {out}");
        assert!(out.contains("序号273"), "应含序号: {out}");
        assert!(out.contains("如意甘肃"), "应含中文名: {out}");
    }

    #[test]
    fn profile_parse_zero_flag() {
        // [0]=0x00 的 PROFILE（日志截断 64B）
        let s = "00000000a2010000fe060000a3000000a0cdcc3f0000000000000000abab0000230506002305060085b50382a0cdcc3fa42aca3f000000000000c08a0000e9db";
        let out = parse_profile(&hex::decode(s).unwrap());
        assert!(out.contains("id="), "应含 id: {out}");
        assert!(out.contains("u32="), "应含 u32 列表: {out}");
    }

    #[test]
    fn qso_parse() {
        // 真实 QSO 样本：BI1FRI / OM89av / 如意甘肃 / ts 1786318787
        let s = "01000000000000000000000015060000c30f796a00000000424931465249000000000000000000004f4d383961760000e5a682e6848fe79498e8828300000000";
        let out = parse_qso(&hex::decode(s).unwrap());
        assert!(out.contains("uid=1557"), "应含 uid: {out}");
        assert!(out.contains("BI1FRI"), "应含对方呼号: {out}");
        assert!(out.contains("OM89av"), "应含网格: {out}");
        assert!(out.contains("时间 08-"), "应含人类可读时间: {out}");
    }
}
