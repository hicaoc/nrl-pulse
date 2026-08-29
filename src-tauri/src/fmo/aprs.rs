//! APRS-IS TCP 客户端 + FMO-V4 广播解析 + 服务器表。

use crate::fmo::protocol;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};

pub const DEFAULT_HOST: &str = "rotate.aprs2.net";
pub const DEFAULT_PORT: u16 = 10152;
/// 上行（发送）专用连接：10152 全馈端口只读，发送需 verified 登录 14580
pub const TX_HOST: &str = "rotate.aprs2.net";
pub const TX_PORT: u16 = 14580;

pub type EmitFn = Arc<dyn Fn(serde_json::Value) + Send + Sync>;
/// 信令消息回调（kind = message/ack 的解析结果），由 QSO 引擎安装
pub type MsgCallback = Arc<std::sync::Mutex<Option<Arc<dyn Fn(serde_json::Value) + Send + Sync>>>>;

const V4_SUBTYPES: &[&[u8]] = &[
    b"STATION", b"ONLINE", b"BEACON", b"VOCAL", b"EVENT", b"JOINT",
];
const LEGACY_SUBTYPES: &[&[u8]] = &[b"OMCQ", b"VOCAL", b"ONLINE", b"BEACON"];
const FMO_MARKERS: &[&[u8]] = &[b"FMO-V4", b"FMO-CLIENT", b"FMO-STATION"];
const SIGNAL_VERBS: &[&[u8]] = &[
    b"QTHQRY",
    b"QTHANS",
    b"CALL",
    b"CALLANS",
    b"CALLCANCEL",
    b"ACCEPT",
    b"REJECT",
    b"BUSY",
    b"DND",
    b"TIMEOUT",
    b"RING",
    b"NOTFRIEND",
    b"NOSERVER",
    b"CONTROL",
    b"NORMAL",
    b"STANDBY",
    b"REBOOT",
];

fn gbk_decode(b: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(b) {
        return s.to_string();
    }
    let (decoded, _enc, _had_errors) = encoding_rs::GBK.decode(b);
    decoded.into_owned()
}

fn is_host(s: &[u8]) -> bool {
    let s = String::from_utf8_lossy(s);
    let host_re =
        regex::Regex::new(r"^(?:[a-zA-Z0-9-]+\.)+[a-zA-Z]{2,}$|^\d{1,3}(?:\.\d{1,3}){3}$").unwrap();
    host_re.is_match(&s)
}

fn is_country(s: &[u8]) -> bool {
    s.len() == 2 && s.iter().all(|b| b.is_ascii_uppercase())
}

fn parse_position_comment(body: &[u8]) -> (serde_json::Value, usize) {
    let re =
        regex::Regex::new(r"^[=!](\d{2})(\d{2}\.\d+)([NS]).(\d{3})(\d{2}\.\d+)([EW]).").unwrap();
    let text = String::from_utf8_lossy(body);
    let mut pos = serde_json::Map::new();
    let mut comment_start = 0usize;
    if let Some(cap) = re.captures(&text) {
        let lat = format!("{}{}{}", &cap[1], &cap[2], &cap[3]);
        let lon = format!("{}{}{}", &cap[4], &cap[5], &cap[6]);
        pos.insert("lat".into(), json!(lat));
        pos.insert("lon".into(), json!(lon));
        comment_start = cap.get(0).unwrap().end();
    }
    (serde_json::Value::Object(pos), comment_start)
}

fn parse_v4(
    callsign: &str,
    dest: &str,
    comment: &[u8],
    raw: &[u8],
    pos: &serde_json::Value,
) -> serde_json::Value {
    let mut out = json!({
        "kind": "broadcast", "version": "FMO-V4",
        "callsign": callsign, "dest": dest,
        "raw": String::from_utf8_lossy(raw).into_owned(),
    });
    if let serde_json::Value::Object(ref m) = pos {
        for (k, v) in m {
            out[k] = v.clone();
        }
    }
    let after_prefix = comment
        .iter()
        .position(|&b| b == b',')
        .unwrap_or(comment.len())
        + 1;
    let rest = &comment[after_prefix.min(comment.len())..];
    let tokens: Vec<&[u8]> = rest.split(|&b| b == b',').collect();
    let mut tokens_iter = tokens.iter();
    let mut extra: Vec<Vec<u8>> = Vec::new();
    if let Some(first) = tokens_iter.next() {
        if V4_SUBTYPES.contains(first) {
            out["subtype"] = json!(String::from_utf8_lossy(first));
        } else {
            extra.push(first.to_vec());
        }
    }
    for tok in tokens_iter {
        let t = *tok;
        if let Some(cert_b64) = t.strip_prefix(b"CERT:") {
            let s = String::from_utf8_lossy(cert_b64).into_owned();
            if let Some(cert) = protocol::decode_fmo_cert(&s) {
                out["cert"] = cert.to_json();
                out["cert_raw"] = json!(hex::encode(&cert.raw));
                out["uid"] = json!(cert.uid);
            }
        } else if let Some(sig) = t.strip_prefix(b"SIG:") {
            out["sig"] = json!(String::from_utf8_lossy(sig));
        } else if let Some(f) = t.strip_prefix(b"FREQ:") {
            if let Ok(v) = String::from_utf8_lossy(f).parse::<f64>() {
                out["freq"] = json!(v);
            }
        } else if let Some(h) = t.strip_prefix(b"HEIGHT:") {
            if let Ok(v) = String::from_utf8_lossy(h).parse::<i64>() {
                out["height"] = json!(v);
            }
        } else if let Some(r) = t.strip_prefix(b"RIG:") {
            out["rig"] = json!(gbk_decode(r));
        } else if let Some(a) = t.strip_prefix(b"ANT:") {
            out["ant"] = json!(gbk_decode(a));
        } else if let Some(sh) = t.strip_prefix(b"SH:") {
            out["host"] = json!(String::from_utf8_lossy(sh));
        } else if t.len() > 1 && t[0] == b'P' {
            let p = &t[1..];
            if p.len() >= 2 && p.len() <= 5 && p.iter().all(|b| b.is_ascii_digit()) {
                if let Ok(v) = String::from_utf8_lossy(p).parse::<i64>() {
                    out["port"] = json!(v);
                }
            }
        } else if t.len() > 3
            && t[0] == b'F'
            && t.ends_with(b"KM")
            && t[1..t.len() - 2].iter().all(|b| b.is_ascii_digit())
        {
            if let Ok(v) = String::from_utf8_lossy(&t[1..t.len() - 2]).parse::<i64>() {
                out["cover_km"] = json!(v);
            }
        } else if t.len() > 1 && t[0] == b'U' {
            let inner = &t[1..];
            if let Some(slash) = inner.iter().position(|&b| b == b'/') {
                let a = &inner[..slash];
                let b = &inner[slash + 1..];
                if let (Ok(online), Ok(total)) = (
                    String::from_utf8_lossy(a).parse::<i64>(),
                    String::from_utf8_lossy(b).parse::<i64>(),
                ) {
                    out["online"] = json!(online);
                    out["total"] = json!(total);
                }
            }
        } else if t.len() > 1 && t[0] == b'S' && t[1..].iter().all(|b| b.is_ascii_digit()) {
            if let Ok(v) = String::from_utf8_lossy(&t[1..]).parse::<i64>() {
                out["s_code"] = json!(v);
            }
        } else {
            extra.push(t.to_vec());
        }
    }
    if out["subtype"] == "STATION" && !extra.is_empty() {
        if is_country(&extra[0]) {
            out["country"] = json!(String::from_utf8_lossy(&extra[0]));
            extra.remove(0);
        }
        if !extra.is_empty() && !is_host(&extra[0]) {
            out["name"] = json!(gbk_decode(&extra[0]));
            extra.remove(0);
        }
        if !extra.is_empty() && is_host(&extra[0]) {
            out["host"] = json!(String::from_utf8_lossy(&extra[0]));
            extra.remove(0);
        }
    } else if !extra.is_empty() {
        for tok in extra {
            if is_host(&tok) {
                out["host"] = json!(String::from_utf8_lossy(&tok));
                break;
            }
        }
    }
    out
}

fn parse_legacy(
    callsign: &str,
    dest: &str,
    marker: &[u8],
    comment: &[u8],
    raw: &[u8],
    pos: &serde_json::Value,
) -> serde_json::Value {
    let mut out = json!({
        "kind": "broadcast", "version": "legacy",
        "callsign": callsign, "dest": dest,
        "legacy_type": String::from_utf8_lossy(marker),
        "raw": String::from_utf8_lossy(raw).into_owned(),
    });
    if let serde_json::Value::Object(ref m) = pos {
        for (k, v) in m {
            out[k] = v.clone();
        }
    }
    let after_prefix = comment
        .iter()
        .position(|&b| b == b',')
        .unwrap_or(comment.len())
        + 1;
    let rest = &comment[after_prefix.min(comment.len())..];
    for tok in rest.split(|&b| b == b',') {
        if LEGACY_SUBTYPES.contains(&tok) {
            out["subtype"] = json!(String::from_utf8_lossy(tok));
        } else if let Some(sh) = tok.strip_prefix(b"SH:") {
            out["host"] = json!(String::from_utf8_lossy(sh));
        } else if tok.len() > 1 && tok[0] == b'P' {
            let p = &tok[1..];
            if p.len() >= 2 && p.len() <= 5 && p.iter().all(|b| b.is_ascii_digit()) {
                if let Ok(v) = String::from_utf8_lossy(p).parse::<i64>() {
                    out["port"] = json!(v);
                }
            }
        } else if let Some(f) = tok.strip_prefix(b"FREQ:") {
            if let Ok(v) = String::from_utf8_lossy(f).parse::<f64>() {
                out["freq"] = json!(v);
            }
        } else if let Some(h) = tok.strip_prefix(b"HEIGHT:") {
            if let Ok(v) = String::from_utf8_lossy(h).parse::<i64>() {
                out["height"] = json!(v);
            }
        } else if let Some(r) = tok.strip_prefix(b"RIG:") {
            out["rig"] = json!(gbk_decode(r));
        } else if let Some(a) = tok.strip_prefix(b"ANT:") {
            out["ant"] = json!(gbk_decode(a));
        } else if let Some(pass) = tok.strip_prefix(b"PASS:") {
            out["pass"] = json!(String::from_utf8_lossy(pass));
        } else if let Some(sign) = tok.strip_prefix(b"SIGN:") {
            out["sign"] = json!(String::from_utf8_lossy(sign));
        } else if !tok.is_empty() && tok.len() <= 6 && tok.iter().all(|b| b.is_ascii_digit()) {
            if out.get("seq").is_none() {
                if let Ok(v) = String::from_utf8_lossy(tok).parse::<i64>() {
                    out["seq"] = json!(v);
                }
            }
        }
    }
    out
}

fn parse_message(callsign: &str, dest: &str, body: &[u8], raw: &[u8]) -> Option<serde_json::Value> {
    if body.len() < 12 {
        return None;
    }
    let to = String::from_utf8_lossy(&body[1..10]).trim().to_string();
    let mut text = body[11..].to_vec();
    let mut msg_id = None;
    if let Some(i) = text.iter().rposition(|&b| b == b'{') {
        let mid: Vec<u8> = text.drain(i..).skip(1).collect();
        msg_id = Some(String::from_utf8_lossy(&mid).into_owned());
    }
    let tokens: Vec<Vec<u8>> = text.split(|&b| b == b',').map(|t| t.to_vec()).collect();
    let verb = tokens[0].clone();
    if SIGNAL_VERBS.contains(&verb.as_slice()) {
        let mut out = json!({
            "kind": "message", "callsign": callsign, "dest": dest,
            "to": to, "verb": String::from_utf8_lossy(&verb),
            "fields": tokens[1..].iter()
                .map(|t| json!(String::from_utf8_lossy(t))).collect::<Vec<_>>(),
            "raw": String::from_utf8_lossy(raw).into_owned(),
        });
        if let Some(id) = msg_id {
            out["msg_id"] = json!(id);
        }
        for t in &tokens[1..] {
            if t.len() > 1 && t[0] == b'U' && t[1..].iter().all(|b| b.is_ascii_digit()) {
                if let Ok(v) = String::from_utf8_lossy(&t[1..]).parse::<i64>() {
                    out["uid"] = json!(v);
                }
            }
        }
        return Some(out);
    }
    if let Some(stripped) = verb.strip_prefix(b"ack") {
        if stripped.is_empty() || stripped.iter().all(|b| b.is_ascii_digit()) {
            let mut out = json!({
                "kind": "ack", "callsign": callsign, "dest": dest,
                "to": to, "raw": String::from_utf8_lossy(raw).into_owned(),
            });
            if let Some(id) = msg_id {
                out["msg_id"] = json!(id);
            }
            return Some(out);
        }
    }
    if dest.starts_with("APFMO") {
        let mut out = json!({
            "kind": "message", "callsign": callsign, "dest": dest,
            "to": to, "verb": serde_json::Value::Null,
            "text": gbk_decode(&text),
            "raw": String::from_utf8_lossy(raw).into_owned(),
        });
        if let Some(id) = msg_id {
            out["msg_id"] = json!(id);
        }
        return Some(out);
    }
    None
}

/// 解析一行 APRS 文本，是 FMO 相关报文则返回 dict。
pub fn parse_fmo_line(line: &[u8]) -> Option<serde_json::Value> {
    let data = line
        .iter()
        .copied()
        .take_while(|b| *b != b'\n')
        .filter(|b| *b != b'\r')
        .collect::<Vec<u8>>();
    if data.is_empty() || data[0] == b'#' {
        return None;
    }
    let gt = data.iter().position(|&b| b == b'>')?;
    let colon = data.iter().position(|&b| b == b':')?;
    let callsign = String::from_utf8_lossy(&data[..gt]).into_owned();
    let dest_end = data[gt + 1..]
        .iter()
        .position(|&b| b == b',')
        .map(|i| gt + 1 + i)
        .unwrap_or(colon);
    let dest = String::from_utf8_lossy(&data[gt + 1..dest_end]).into_owned();
    let body = &data[colon + 1..];

    if body.starts_with(b">") && dest.starts_with("APFMO") {
        return Some(json!({
            "kind": "status", "callsign": callsign, "dest": dest,
            "status_text": gbk_decode(&body[1..]),
            "raw": String::from_utf8_lossy(&data).into_owned(),
        }));
    }
    if body.starts_with(b":") {
        return parse_message(&callsign, &dest, body, &data);
    }
    let (pos, comment_start) = parse_position_comment(body);
    if dest == "APFMO2" {
        let text = gbk_decode(&body[comment_start..]);
        let mut out = json!({
            "kind": "client_beacon", "callsign": callsign, "dest": dest,
            "comment": text,
            "raw": String::from_utf8_lossy(&data).into_owned(),
        });
        if let serde_json::Value::Object(ref m) = pos {
            for (k, v) in m {
                out[k] = v.clone();
            }
        }
        let re = regex::Regex::new(r"\.FMO\s+(\d+)").unwrap();
        if let Some(cap) = re.captures(&text) {
            if let Ok(uid) = cap[1].parse::<i64>() {
                out["uid"] = json!(uid);
            }
        }
        return Some(out);
    }
    let mut idx: Option<usize> = None;
    let mut marker: Option<&[u8]> = None;
    for mk in FMO_MARKERS {
        if let Some(i) = find_bytes(body, mk) {
            if idx.map(|cur| i < cur).unwrap_or(true) {
                idx = Some(i);
                marker = Some(*mk);
            }
        }
    }
    let Some(comment_start) = idx else {
        let re = regex::Regex::new(r"\.FMO\s+(\d+)").unwrap();
        let text = String::from_utf8_lossy(body);
        if let Some(cap) = re.captures(&text) {
            if let Ok(uid) = cap[1].parse::<i64>() {
                let mut out = json!({
                    "kind": "position", "version": "legacy",
                    "callsign": callsign, "dest": dest, "uid": uid,
                    "raw": String::from_utf8_lossy(&data).into_owned(),
                });
                if let serde_json::Value::Object(ref m) = pos {
                    for (k, v) in m {
                        out[k] = v.clone();
                    }
                }
                return Some(out);
            }
        }
        return None;
    };
    let comment = &body[comment_start..];
    let mk = marker.unwrap();
    if mk == b"FMO-V4" {
        Some(parse_v4(&callsign, &dest, comment, &data, &pos))
    } else {
        Some(parse_legacy(&callsign, &dest, mk, comment, &data, &pos))
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------- 服务器表

pub struct ServerTable {
    pub servers: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    /// FMO 用户（客户端设备）表：key = 呼号大写，仅内存，随信标实时更新
    pub clients: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    pub persist_path: Option<PathBuf>,
    /// STATION 广播 upsert 后的回调（FmoState 安装，用于选定服务器证书自愈刷新）
    pub on_upsert: Arc<std::sync::Mutex<Option<Arc<dyn Fn(serde_json::Value) + Send + Sync>>>>,
}

impl ServerTable {
    pub fn new(persist_path: Option<PathBuf>) -> Self {
        let mut servers = HashMap::new();
        if let Some(p) = &persist_path {
            if let Ok(text) = std::fs::read_to_string(p) {
                if let Ok(list) = serde_json::from_str::<Vec<serde_json::Value>>(&text) {
                    for s in list {
                        // 旧格式 key = "host|callsign"，迁移为 "host:port"；跳过 host 为空条目
                        let mut entry = s;
                        let host = entry
                            .get("host")
                            .and_then(|h| h.as_str())
                            .unwrap_or("")
                            .to_string();
                        if host.is_empty() {
                            continue;
                        }
                        let port = entry.get("port").and_then(|p| p.as_u64()).unwrap_or(1883);
                        let key = format!("{host}:{port}");
                        entry["key"] = json!(key);
                        entry["port"] = json!(port);
                        servers.insert(key.to_string(), entry);
                    }
                }
            }
        }
        Self {
            servers: Arc::new(Mutex::new(servers)),
            clients: Arc::new(Mutex::new(HashMap::new())),
            persist_path,
            on_upsert: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    fn save_locked(&self, servers: &HashMap<String, serde_json::Value>) {
        let Some(p) = &self.persist_path else { return };
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let list: Vec<&serde_json::Value> = servers.values().collect();
        if let Ok(text) = serde_json::to_string_pretty(&list) {
            std::fs::write(p.with_extension("tmp"), text).ok();
            let _ = std::fs::rename(p.with_extension("tmp"), p);
        }
    }

    pub async fn upsert(
        &self,
        parsed: &serde_json::Value,
        source: &str,
    ) -> Option<serde_json::Value> {
        let host = parsed
            .get("host")
            .and_then(|h| h.as_str())
            .unwrap_or("")
            .to_string();
        let callsign = parsed
            .get("callsign")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        // 只记录可连接的服务器（host 非空），ONLINE/BEACON/VOCAL/status 等无 host 广播不建条目
        if host.is_empty() {
            return None;
        }
        let now = chrono::Utc::now().timestamp();
        let raw = parsed
            .get("raw")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        // 以 host:port 为唯一键，避免同一服务器被多个中继重复广播产生多条
        let port = parsed.get("port").and_then(|p| p.as_u64()).unwrap_or(1883);
        let key = format!("{host}:{port}");
        let mut servers = self.servers.lock().await;
        let existing = servers.get(&key).cloned();
        let mut entry = if let Some(mut e) = existing {
            e["last_seen"] = json!(now);
            e["raw"] = json!(raw);
            if !host.is_empty() {
                e["host"] = json!(host);
                e["port"] = json!(port);
            }
            e
        } else {
            json!({
                "key": key, "host": host, "port": parsed.get("port").cloned().unwrap_or(json!(1883)),
                "callsign": callsign, "name": parsed.get("name").unwrap_or(&json!("")),
                "source": source, "first_seen": now, "last_seen": now,
                "raw": raw,
            })
        };
        for f in [
            "port",
            "online",
            "total",
            "cover_km",
            "freq",
            "height",
            "uid",
            "subtype",
            "version",
            "name",
            "s_code",
            "country",
            "status_text",
            "cert",
            "lat",
            "lon",
            "rig",
            "ant",
        ] {
            if let Some(v) = parsed.get(f) {
                if !v.is_null() {
                    entry[f] = v.clone();
                }
            }
        }
        // 若已有该服务器条目，更新中继呼号信息（保留最近一次广播的呼号）
        if let Some(cs) = parsed.get("callsign") {
            if !cs.is_null() {
                entry["callsign"] = cs.clone();
            }
        }
        servers.insert(key.clone(), entry.clone());
        self.save_locked(&servers);
        drop(servers);
        // Notify after releasing the table lock: the callback may lock
        // selected_server / reconnect MQTT and must not run under this lock.
        if let Some(cb) = self.on_upsert.lock().unwrap().clone() {
            cb(entry.clone());
        }
        Some(entry)
    }

    pub async fn to_list(&self) -> Vec<serde_json::Value> {
        let servers = self.servers.lock().await;
        // 只保留可连接的服务器：host 非空
        let mut list: Vec<serde_json::Value> = servers
            .values()
            .filter(|s| {
                s.get("host")
                    .and_then(|h| h.as_str())
                    .map(|h| !h.is_empty())
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        list.sort_by_key(|s| {
            let last = s.get("last_seen").and_then(|v| v.as_i64()).unwrap_or(0);
            std::cmp::Reverse(last)
        });
        list
    }

    /// 记录 FMO 用户（客户端设备）信标：client_beacon / position / status 类，
    /// 以呼号（大写）为唯一键。返回更新后的条目；无呼号则忽略。
    pub async fn upsert_client(&self, parsed: &serde_json::Value) -> Option<serde_json::Value> {
        let callsign = parsed
            .get("callsign")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        if callsign.is_empty() {
            return None;
        }
        let key = callsign.to_uppercase();
        let now = chrono::Utc::now().timestamp();
        let mut clients = self.clients.lock().await;
        let mut entry = if let Some(e) = clients.get(&key).cloned() {
            e
        } else {
            json!({ "callsign": key, "first_seen": now })
        };
        entry["last_seen"] = json!(now);
        entry["kind"] = parsed.get("kind").cloned().unwrap_or(json!(""));
        // STATUS 消息：按时间倒序保留最近 2 条（供用户列表「最近消息」展示）
        if parsed.get("kind").and_then(|k| k.as_str()) == Some("status") {
            if let Some(text) = parsed.get("status_text").and_then(|s| s.as_str()) {
                if !text.is_empty() {
                    let mut recent = entry
                        .get("recent")
                        .and_then(|r| r.as_array())
                        .cloned()
                        .unwrap_or_default();
                    recent.insert(0, json!({ "ts": now, "text": text }));
                    recent.truncate(2);
                    entry["recent"] = json!(recent);
                }
            }
        }
        for f in [
            "uid",
            "subtype",
            "status_text",
            "comment",
            "freq",
            "rig",
            "version",
            "lat",
            "lon",
            "height",
            "ant",
        ] {
            if let Some(v) = parsed.get(f) {
                if !v.is_null() {
                    entry[f] = v.clone();
                }
            }
        }
        clients.insert(key, entry.clone());
        // 上限保护：超过 1000 个用户时淘汰最久未见的
        if clients.len() > 1000 {
            let oldest = clients
                .iter()
                .min_by_key(|(_, v)| v.get("last_seen").and_then(|t| t.as_i64()).unwrap_or(0))
                .map(|(k, _)| k.clone());
            if let Some(k) = oldest {
                clients.remove(&k);
            }
        }
        Some(entry)
    }

    /// 按呼号（忽略 SSID/大小写）在用户表/服务器表查 uid（QSO 呼叫解析目标 uid 用）
    pub async fn lookup_uid_by_callsign(&self, callsign: &str) -> Option<u32> {
        let key = callsign
            .split('-')
            .next()
            .unwrap_or(callsign)
            .to_uppercase();
        {
            let clients = self.clients.lock().await;
            for (k, v) in clients.iter() {
                let base = k.split('-').next().unwrap_or(k);
                if base == key {
                    if let Some(u) = v.get("uid").and_then(|u| u.as_u64()) {
                        return Some(u as u32);
                    }
                }
            }
        }
        let servers = self.servers.lock().await;
        for v in servers.values() {
            let cs = v.get("callsign").and_then(|c| c.as_str()).unwrap_or("");
            if cs.split('-').next().unwrap_or(cs).to_uppercase() == key {
                if let Some(u) = v.get("uid").and_then(|u| u.as_u64()) {
                    return Some(u as u32);
                }
            }
        }
        None
    }

    /// 按 uid 查服务器条目（QSO 跳台：QTHANS 的 S<服务器uid> → host/port/证书）
    pub async fn find_server_by_uid(&self, uid: u32) -> Option<serde_json::Value> {
        let servers = self.servers.lock().await;
        servers
            .values()
            .find(|v| v.get("uid").and_then(|u| u.as_u64()) == Some(uid as u64))
            .cloned()
    }

    pub async fn client_list(&self) -> Vec<serde_json::Value> {
        // 服务器（STATION 广播带 host）也会发 STATUS/位置报文，按其呼号把服务器从用户表中剔除
        let servers = self.servers.lock().await;
        let mut server_calls: std::collections::HashSet<String> = std::collections::HashSet::new();
        for s in servers.values() {
            if let Some(c) = s.get("callsign").and_then(|c| c.as_str()) {
                let up = c.to_uppercase();
                if !up.is_empty() {
                    server_calls.insert(up.clone());
                    if let Some(base) = up.split('-').next() {
                        server_calls.insert(base.to_string());
                    }
                }
            }
        }
        drop(servers);
        let clients = self.clients.lock().await;
        let mut list: Vec<serde_json::Value> = clients
            .values()
            .filter(|c| {
                let cs = c
                    .get("callsign")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_uppercase();
                let base = cs.split('-').next().unwrap_or("");
                !cs.is_empty() && !server_calls.contains(&cs) && !server_calls.contains(base)
            })
            .cloned()
            .collect();
        list.sort_by_key(|s| {
            let last = s.get("last_seen").and_then(|v| v.as_i64()).unwrap_or(0);
            std::cmp::Reverse(last)
        });
        list
    }
}

// ---------------------------------------------------------------- APRS-IS 客户端

pub struct AprsClient {
    pub emit: EmitFn,
    pub table: Arc<ServerTable>,
    pub state: Arc<Mutex<String>>,
    pub detail: Arc<Mutex<String>>,
    pub connect_req: Arc<Mutex<Option<serde_json::Value>>>,
    pub disconnect_signal: Arc<Mutex<bool>>,
    /// 信令消息回调（QSO 引擎安装），主连接与上行连接收到的都会投递
    pub on_message: MsgCallback,
    /// 上行（发送）专用连接
    pub tx: Arc<AprsTx>,
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AprsParams {
    pub host: String,
    pub port: u16,
    pub callsign: String,
    pub passcode: String,
    pub lat: f64,
    pub lon: f64,
    pub dist: f64,
}

impl AprsClient {
    pub fn new(emit: EmitFn, table: Arc<ServerTable>) -> Self {
        let on_message: MsgCallback = Arc::new(std::sync::Mutex::new(None));
        Self {
            emit: emit.clone(),
            table,
            state: Arc::new(Mutex::new("disconnected".into())),
            detail: Arc::new(Mutex::new(String::new())),
            connect_req: Arc::new(Mutex::new(None)),
            disconnect_signal: Arc::new(Mutex::new(false)),
            on_message: on_message.clone(),
            tx: Arc::new(AprsTx::new(emit, on_message)),
        }
    }

    pub async fn connect_to(&self, params: AprsParams) {
        // 有有效 passcode 时同步设置上行登录（verified 才可发送）
        self.tx.set_login(&params.callsign, &params.passcode).await;
        *self.connect_req.lock().await = Some(serde_json::to_value(params).unwrap());
        *self.disconnect_signal.lock().await = false;
    }

    pub async fn disconnect(&self) {
        *self.disconnect_signal.lock().await = true;
        self.tx.clear_login().await;
    }

    async fn set_state(&self, state: &str, detail: &str) {
        *self.state.lock().await = state.to_string();
        *self.detail.lock().await = detail.to_string();
        (self.emit)(json!({"type": "aprs_state", "state": state, "detail": detail}));
    }

    pub async fn run(&self) {
        let mut backoff = 1.0f64;
        loop {
            let want_connect =
                self.connect_req.lock().await.is_some() && !*self.disconnect_signal.lock().await;
            if !want_connect {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                continue;
            }
            let params: AprsParams = match &*self.connect_req.lock().await {
                Some(v) => serde_json::from_value(v.clone()).unwrap_or_default(),
                None => continue,
            };
            if let Err(e) = self.session(params.clone()).await {
                self.set_state("disconnected", &format!("{e}；{backoff:.0}s 后重连"))
                    .await;
            }
            if *self.disconnect_signal.lock().await {
                self.set_state("disconnected", "用户断开").await;
                continue;
            }
            tokio::time::sleep(std::time::Duration::from_secs_f64(backoff)).await;
            backoff = (backoff * 2.0).min(60.0);
        }
    }

    async fn session(&self, p: AprsParams) -> Result<(), String> {
        self.set_state("connecting", &format!("{}:{}", p.host, p.port))
            .await;
        let addr = format!("{}:{}", p.host, p.port);
        let mut stream = TcpStream::connect(&addr).await.map_err(|e| e.to_string())?;
        stream.set_nodelay(true).ok();
        let login = if p.port == 10152 {
            format!(
                "user {} pass {} vers FMO-SIM 1.0\r\n",
                p.callsign, p.passcode
            )
        } else {
            format!(
                "user {} pass {} vers APFMO4 filter r/{}/{}/{}\r\n",
                p.callsign, p.passcode, p.lat, p.lon, p.dist
            )
        };
        stream
            .write_all(login.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let (read_half, _write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut line: Vec<u8> = Vec::new();
        let mut n_lines = 0usize;
        let mut n_fmo = 0usize;
        let mut t_stats = chrono::Utc::now().timestamp_millis();
        loop {
            if *self.disconnect_signal.lock().await {
                return Ok(());
            }
            line.clear();
            let n = reader
                .read_until(b'\n', &mut line)
                .await
                .map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("对端关闭连接".into());
            }
            n_lines += 1;
            let bytes: Vec<u8> = line
                .iter()
                .copied()
                .filter(|b| *b != b'\r' && *b != b'\n')
                .collect();
            let raw = String::from_utf8_lossy(&bytes);
            let raw_bytes: Vec<u8> = bytes.clone();
            if raw.starts_with("# logresp") {
                let detail = raw.to_string();
                (self.emit)(
                    json!({"type": "log", "level": "info", "msg": format!("APRS-IS: {detail}")}),
                );
                if raw.contains("unverified") {
                    self.set_state("listen-only", &detail).await;
                } else if raw.contains("verified") {
                    self.set_state("verified", &detail).await;
                } else {
                    self.set_state("logged-in", &detail).await;
                }
                continue;
            }
            if raw.starts_with('#') {
                continue;
            }
            let is_fmoish = raw.contains("FMO");
            if let Some(parsed) = parse_fmo_line(&raw_bytes) {
                n_fmo += 1;
                let source = parsed
                    .get("callsign")
                    .and_then(|c| c.as_str())
                    .unwrap_or("aprs");
                let entry = self.table.upsert(&parsed, source).await;
                let kind = parsed.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                // 信令/ACK 投递给 QSO 引擎（去重由引擎负责，上行连接也会收到同一消息）
                if matches!(kind, "message" | "ack") {
                    let cb = self.on_message.lock().unwrap().clone();
                    if let Some(cb) = cb {
                        cb(parsed.clone());
                    }
                }
                match kind {
                    "status" => {
                        (self.emit)(json!({"type": "log", "level": "info",
                            "msg": format!("FMO 状态 {}: {}",
                                           parsed.get("callsign").unwrap(),
                                           parsed.get("status_text").and_then(|s| s.as_str()).unwrap_or(""))}));
                    }
                    _ if parsed.get("subtype").and_then(|s| s.as_str()) == Some("STATION") => {
                        (self.emit)(json!({"type": "log", "level": "info",
                            "msg": format!("服务器广播 {}: {} {}:{} 在线{}/{} uid={}",
                                           parsed.get("callsign").unwrap(),
                                           parsed.get("name").unwrap_or(&json!("")),
                                           parsed.get("host").unwrap_or(&json!("")),
                                           parsed.get("port").unwrap_or(&json!("")),
                                           parsed.get("online").unwrap_or(&json!("?")),
                                           parsed.get("total").unwrap_or(&json!("?")),
                                           parsed.get("uid").unwrap_or(&json!("")))}));
                    }
                    "message" => {
                        let verb = parsed.get("verb").and_then(|v| v.as_str()).unwrap_or("");
                        if verb.is_empty() {
                            // 自由文本消息（无 verb）：打 text，按字符截断 60 避免截断 UTF-8
                            let text: String = parsed
                                .get("text")
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .chars()
                                .take(60)
                                .collect();
                            (self.emit)(json!({"type": "log", "level": "info",
                                "msg": format!("客户端消息 {}→{}: {}",
                                               parsed.get("callsign").unwrap(),
                                               parsed.get("to").unwrap_or(&json!("")),
                                               text)}));
                        } else {
                            let fields: Vec<String> = parsed
                                .get("fields")
                                .map(|f| {
                                    f.as_array()
                                        .unwrap_or(&vec![])
                                        .iter()
                                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                        .collect()
                                })
                                .unwrap_or_default();
                            (self.emit)(json!({"type": "log", "level": "info",
                                "msg": format!("FMO 信令 {}→{}: {} {}",
                                               parsed.get("callsign").unwrap(),
                                               parsed.get("to").unwrap_or(&json!("")),
                                               verb, fields.join(" "))}));
                        }
                    }
                    "ack" => {
                        (self.emit)(json!({"type": "log", "level": "info",
                            "msg": format!("FMO ACK {}→{} #{}",
                                           parsed.get("callsign").unwrap(),
                                           parsed.get("to").unwrap_or(&json!("")),
                                           parsed.get("msg_id").unwrap_or(&json!("")))}));
                    }
                    "client_beacon" => {
                        let comment: String = parsed
                            .get("comment")
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .chars()
                            .take(60)
                            .collect();
                        (self.emit)(json!({"type": "log", "level": "info",
                            "msg": format!("客户端备注 {}: {}",
                                           parsed.get("callsign").unwrap(),
                                           comment)}));
                    }
                    "broadcast" => {
                        // 客户端信标（V4 BEACON/ONLINE/VOCAL、legacy FMO-CLIENT 等）：
                        // 此类报文本就不含 host/port/U 字段，按信标格式打
                        let mut info: Vec<String> = Vec::new();
                        if let Some(uid) = parsed.get("uid").filter(|v| !v.is_null()) {
                            info.push(format!("uid={uid}"));
                        } else if let Some(s) = parsed.get("s_code").filter(|v| !v.is_null()) {
                            info.push(format!("S{s}"));
                        }
                        if let Some(f) = parsed.get("freq").filter(|v| !v.is_null()) {
                            info.push(format!("{f}MHz"));
                        }
                        if let Some(h) = parsed.get("height").filter(|v| !v.is_null()) {
                            info.push(format!("高度{h}m"));
                        }
                        if let Some(rig) = parsed.get("rig").and_then(|r| r.as_str()) {
                            if !rig.is_empty() {
                                info.push(rig.to_string());
                            }
                        }
                        let label = parsed
                            .get("subtype")
                            .and_then(|s| s.as_str())
                            .or_else(|| parsed.get("version").and_then(|v| v.as_str()))
                            .unwrap_or("");
                        let mut msg = format!(
                            "客户端信标 {} [{}]",
                            parsed.get("callsign").unwrap(),
                            label
                        );
                        if !info.is_empty() {
                            msg.push(' ');
                            msg.push_str(&info.join(" "));
                        }
                        (self.emit)(json!({"type": "log", "level": "info", "msg": msg}));
                    }
                    _ => {
                        // position（.FMO <uid>）等其余类型：按实际字段打，不假装有 host/port/U
                        (self.emit)(json!({"type": "log", "level": "info",
                            "msg": format!("FMO {} {}: uid={} {} {}",
                                           kind,
                                           parsed.get("callsign").unwrap(),
                                           parsed.get("uid").unwrap_or(&json!("")),
                                           parsed.get("lat").unwrap_or(&json!("")),
                                           parsed.get("lon").unwrap_or(&json!("")))}));
                    }
                }
                if entry.is_some() {
                    (self.emit)(json!({"type": "server_list",
                        "servers": self.table.to_list().await}));
                }
                // 用户（客户端设备）信标：client_beacon / position / status 更新用户表；
                // 另含无 host 的客户端广播（V4 BEACON、老版 FMO-CLIENT，携带 FREQ/HEIGHT/RIG/ANT）
                let no_host = parsed
                    .get("host")
                    .and_then(|h| h.as_str())
                    .map(|h| h.is_empty())
                    .unwrap_or(true);
                let is_client_broadcast = kind == "broadcast"
                    && no_host
                    && (parsed.get("subtype").and_then(|s| s.as_str()) == Some("BEACON")
                        || parsed.get("legacy_type").and_then(|s| s.as_str())
                            == Some("FMO-CLIENT"));
                if (matches!(kind, "client_beacon" | "position" | "status") || is_client_broadcast)
                    && self.table.upsert_client(&parsed).await.is_some()
                {
                    (self.emit)(json!({"type": "client_list",
                        "clients": self.table.client_list().await}));
                }
            } else if is_fmoish {
                n_fmo += 1;
                let preview: String = raw.chars().take(150).collect();
                (self.emit)(json!({"type": "log", "level": "warn",
                    "msg": format!("FMO 未解析: {preview}")}));
            }
            let now = chrono::Utc::now().timestamp_millis();
            if now - t_stats >= 60_000 {
                t_stats = now;
                (self.emit)(json!({"type": "log", "level": "info",
                    "msg": format!("APRS 收包统计：共 {n_lines} 行，FMO 相关 {n_fmo} 条")}));
            }
        }
    }
}

// ---------------------------------------------------------------- APRS-IS 上行连接（发送专用）

/// 发送专用连接：登录 14580（不带 filter，只收发给本机呼号的消息，作为信令接收双保险）。
/// 主连接保持 10152 全馈用于全球服务器发现，两条连接互不影响。
pub struct AprsTx {
    emit: EmitFn,
    /// disconnected / connecting / verified / listen-only
    pub state: Arc<Mutex<String>>,
    login: Arc<Mutex<Option<(String, String)>>>,
    sender: Arc<Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>>,
    on_message: MsgCallback,
}

impl AprsTx {
    fn new(emit: EmitFn, on_message: MsgCallback) -> Self {
        Self {
            emit,
            state: Arc::new(Mutex::new("disconnected".into())),
            login: Arc::new(Mutex::new(None)),
            sender: Arc::new(Mutex::new(None)),
            on_message,
        }
    }

    pub async fn set_login(&self, callsign: &str, passcode: &str) {
        let cs = callsign.trim();
        let pc = passcode.trim();
        if cs.is_empty() || pc.is_empty() || pc == "-1" {
            return;
        }
        *self.login.lock().await = Some((cs.to_uppercase(), pc.to_string()));
    }

    pub async fn clear_login(&self) {
        *self.login.lock().await = None;
    }

    /// 排队发送一行 APRS 报文（不含换行）。上行未连接/未验证时返回错误。
    pub async fn send_packet(&self, line: Vec<u8>) -> Result<(), String> {
        if *self.state.lock().await != "verified" {
            return Err("APRS 上行未验证登录（需要正确的 passcode），暂不能发送".into());
        }
        let tx = self.sender.lock().await.clone();
        match tx {
            Some(tx) => tx.send(line).map_err(|_| "APRS 上行连接已断开".into()),
            None => Err("APRS 上行未连接".into()),
        }
    }

    pub async fn run(&self) {
        let mut backoff = 1.0f64;
        loop {
            let login = self.login.lock().await.clone();
            let Some((cs, pc)) = login else {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                continue;
            };
            match self.session(&cs, &pc).await {
                Ok(()) => {
                    *self.state.lock().await = "disconnected".into();
                }
                Err(e) => {
                    *self.state.lock().await = "disconnected".into();
                    (self.emit)(json!({"type": "log", "level": "warn",
                        "msg": format!("APRS 上行断开：{e}；{backoff:.0}s 后重连")}));
                }
            }
            *self.sender.lock().await = None;
            // 登录信息被清除（用户断开 APRS）时不再重连
            if self.login.lock().await.is_none() {
                continue;
            }
            tokio::time::sleep(std::time::Duration::from_secs_f64(backoff)).await;
            backoff = (backoff * 2.0).min(60.0);
        }
    }

    async fn session(&self, callsign: &str, passcode: &str) -> Result<(), String> {
        *self.state.lock().await = "connecting".into();
        let addr = format!("{TX_HOST}:{TX_PORT}");
        let stream = TcpStream::connect(&addr).await.map_err(|e| e.to_string())?;
        stream.set_nodelay(true).ok();
        let login = format!("user {callsign} pass {passcode} vers NRL-PULSE 1.0\r\n");
        let (read_half, mut write_half) = stream.into_split();
        write_half
            .write_all(login.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        *self.sender.lock().await = Some(tx);
        let mut reader = BufReader::new(read_half);
        let mut line: Vec<u8> = Vec::new();
        loop {
            if self.login.lock().await.is_none() {
                return Ok(());
            }
            tokio::select! {
                n = reader.read_until(b'\n', &mut line) => {
                    let n = n.map_err(|e| e.to_string())?;
                    if n == 0 {
                        return Err("对端关闭连接".into());
                    }
                    let bytes: Vec<u8> = line.iter().copied()
                        .filter(|b| *b != b'\r' && *b != b'\n').collect();
                    line.clear();
                    let raw = String::from_utf8_lossy(&bytes);
                    if raw.starts_with("# logresp") {
                        if raw.contains("unverified") {
                            *self.state.lock().await = "listen-only".into();
                            (self.emit)(json!({"type": "log", "level": "warn",
                                "msg": "APRS 上行登录未验证（passcode 不对），QSO/广播发送不可用"}));
                        } else if raw.contains("verified") {
                            *self.state.lock().await = "verified".into();
                            (self.emit)(json!({"type": "log", "level": "info",
                                "msg": "APRS 上行已验证，QSO 信令与服务器广播可发送"}));
                        }
                        continue;
                    }
                    if raw.starts_with('#') {
                        continue;
                    }
                    if let Some(parsed) = parse_fmo_line(&bytes) {
                        let kind = parsed.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                        if matches!(kind, "message" | "ack") {
                            let cb = self.on_message.lock().unwrap().clone();
                            if let Some(cb) = cb {
                                cb(parsed);
                            }
                        }
                    }
                }
                pkt = rx.recv() => {
                    let Some(mut pkt) = pkt else {
                        return Ok(());
                    };
                    pkt.extend_from_slice(b"\r\n");
                    write_half.write_all(&pkt).await.map_err(|e| e.to_string())?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_v4_station() {
        let line = b"BG9JYT>APFMO4,TCPIP*,qAC,T2FMO:=3952.80NF11931.57EiFMO-V4,STATION,CERT:eJzjYmBgYEowyMzMTAAxGQwAAf0D-g,BG9JYT,fmo.srv.ink,P1883,F500KM,U12/100,SIG:abc";
        let parsed = parse_fmo_line(line);
        assert!(parsed.is_some(), "should parse");
        let p = parsed.unwrap();
        assert_eq!(p["kind"], "broadcast");
        assert_eq!(p["version"], "FMO-V4");
        assert_eq!(p["callsign"], "BG9JYT");
    }

    #[test]
    fn parse_status() {
        let line = "BG9JYT>APFMO1,TCPIP*:>服务器在线".as_bytes();
        let parsed = parse_fmo_line(line);
        assert!(parsed.is_some());
        let p = parsed.unwrap();
        assert_eq!(p["kind"], "status");
    }

    #[test]
    fn parse_message() {
        let line = b"BG9JYT>APFMO2,TCPIP*::BG5ESN   :CALL,447{12345";
        let parsed = parse_fmo_line(line);
        assert!(parsed.is_some());
        let p = parsed.unwrap();
        assert_eq!(p["kind"], "message");
        assert_eq!(p["verb"], "CALL");
    }

    #[test]
    fn gbk_name_decode() {
        let name = gbk_decode(&[0xd7, 0xcd, 0xb2, 0xa9]);
        assert_eq!(name, "淄博");
    }

    #[test]
    fn parse_client_beacon_uid() {
        let line = b"BD4XGT>APFMO2,TCPIP*:=2946.37N10631.18E#.FMO 796 QTH\xc4\xe3\xba\xc3";
        let parsed = parse_fmo_line(line);
        assert!(parsed.is_some());
        let p = parsed.unwrap();
        assert_eq!(p["kind"], "client_beacon");
        assert_eq!(p["uid"], 796);
    }

    #[test]
    fn parse_v4_client_beacon_fields() {
        // V4 客户端信标：BEACON 子类型，携带 FREQ/HEIGHT/RIG/ANT（用户列表详情来源）
        let line = b"BI1SQH-15>APFMO4,TCPIP*,qAC,T2CS:=3953.80NF11633.56EiFMO-V4,BEACON,CERT:eJzjYmBgYEowyMzMTAAxGQwAAf0D-g,FREQ:433.2000,HEIGHT:72,RIG:TK-308,ANT:QTH,SIG:abc";
        let parsed = parse_fmo_line(line);
        assert!(parsed.is_some(), "should parse");
        let p = parsed.unwrap();
        assert_eq!(p["kind"], "broadcast");
        assert_eq!(p["subtype"], "BEACON");
        assert_eq!(p["callsign"], "BI1SQH-15");
        assert_eq!(p["freq"], 433.2);
        assert_eq!(p["height"], 72);
        assert_eq!(p["rig"], "TK-308");
        assert_eq!(p["ant"], "QTH");
        assert!(p.get("host").is_none(), "客户端信标不应有 host");
    }

    #[tokio::test]
    async fn client_beacon_broadcast_populates_user_table() {
        // 端到端：GBK 编码的 V4 BEACON 广播 → 解析 → 用户表，电台/天线等字段应完整保留
        let (rig_gbk, _, _) = encoding_rs::GBK.encode("海能达PDC580");
        let (ant_gbk, _, _) = encoding_rs::GBK.encode("北京朝阳");
        let mut line: Vec<u8> = b"BA4TCS-15>APFMO4,TCPIP*,qAC,T2CS:=3202.39NF12015.69EiFMO-V4,BEACON,CERT:eJzjYmBgYEowyMzMTAAxGQwAAf0D-g,FREQ:431.0000,HEIGHT:18,RIG:".to_vec();
        line.extend_from_slice(&rig_gbk);
        line.extend_from_slice(b",ANT:");
        line.extend_from_slice(&ant_gbk);
        line.extend_from_slice(b",SIG:abc");
        let parsed = parse_fmo_line(&line).expect("should parse");
        assert_eq!(parsed["subtype"], "BEACON");
        let table = ServerTable::new(None);
        let entry = table.upsert_client(&parsed).await.expect("应入用户表");
        assert_eq!(entry["callsign"], "BA4TCS-15");
        assert_eq!(entry["rig"], "海能达PDC580");
        assert_eq!(entry["ant"], "北京朝阳");
        assert_eq!(entry["freq"], 431.0);
        assert_eq!(entry["height"], 18);
        let list = table.client_list().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["rig"], "海能达PDC580");
    }

    #[tokio::test]
    async fn server_table_dedup_by_host_port() {
        let dir = std::env::temp_dir().join(format!("fmo-table-{}", std::process::id()));
        let table = ServerTable::new(Some(dir.join("servers.json")));
        // 同一服务器（fmo.srv.ink:1883）被两个不同中继呼号广播
        let a = json!({
            "host": "fmo.srv.ink", "port": 1883, "callsign": "BG9JYT",
            "name": "测试服务器", "online": 12, "total": 100,
        });
        let b = json!({
            "host": "fmo.srv.ink", "port": 1883, "callsign": "BD4XGT",
            "name": "测试服务器", "online": 15, "total": 100,
        });
        table.upsert(&a, "aprs").await;
        table.upsert(&b, "aprs").await;
        let list = table.to_list().await;
        assert_eq!(
            list.len(),
            1,
            "同一 host:port 应去重，实际 {} 条",
            list.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
