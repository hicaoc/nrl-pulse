//! FMO 服务器登录探测：用当前用户证书对 APRS 发现的服务器做独立 MQTT 登录验证。
//!
//! - 探测会话完全独立（每台服务器一条临时 rumqttc 连接），不影响当前登录会话；
//! - 结果持久化到 probe_results.json（带用户证书指纹）：证书未更换不重复验证，
//!   重启后失败结果保留、不自动复验（提供手动复验）；证书更换 → 全部重验；
//! - 并发上限 PROBE_CONCURRENCY；当前已登录的服务器不探测，直接标记已验证。

use crate::fmo::aprs::{EmitFn, ServerTable};
use crate::fmo::fmo_auth;
use rumqttc::v5::mqttbytes::v5::{ConnectReturnCode, Packet};
use rumqttc::v5::{AsyncClient, Event, MqttOptions};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};

/// 同时进行的探测连接数上限
pub const PROBE_CONCURRENCY: usize = 3;
/// 服务器表扫描周期（新出现/未验证的服务器入队）
const SCAN_INTERVAL_S: u64 = 30;
/// 单次探测：TCP 预检与 ConnAck 等待各自的超时
const TCP_TIMEOUT_S: u64 = 5;
const CONNACK_TIMEOUT_S: u64 = 8;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ProbeRecord {
    /// "ok" | "fail" | "probing"（probing 不落盘）
    pub status: String,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub ts: i64,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct ProbeStore {
    #[serde(default)]
    cert_fp: String,
    #[serde(default)]
    results: HashMap<String, ProbeRecord>,
}

/// 认证被拒之外的候选角色轮换（与主连接 ROLE_SEQ 一致：任一角色能登即算可登录）
const ROLE_SEQ: [&str; 3] = ["user", "super", "admin"];

pub struct ProbeEngine {
    emit: EmitFn,
    certs_dir: PathBuf,
    table: Arc<ServerTable>,
    mqtt_state: Arc<Mutex<String>>,
    selected_server: Arc<Mutex<serde_json::Value>>,
    persist_path: PathBuf,
    store: Arc<Mutex<ProbeStore>>,
    queued: Arc<std::sync::Mutex<HashSet<String>>>,
    sem: Arc<Semaphore>,
    started: Arc<AtomicBool>,
}

/// 当前用户证书指纹（hex）；无证书/读不出时为空串（不探测）。
fn current_cert_fp(certs_dir: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(certs_dir.join("cert_user.json")) else {
        return String::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return String::new();
    };
    hex::encode(fmo_auth::cert_fingerprint(&v))
}

fn load_store(path: &Path) -> ProbeStore {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_store(path: &Path, store: &ProbeStore) {
    // probing 是瞬态，不落盘（重启后视为未验证，由扫描器重新入队）
    let persisted: HashMap<String, ProbeRecord> = store
        .results
        .iter()
        .filter(|(_, r)| r.status != "probing")
        .map(|(k, r)| (k.clone(), r.clone()))
        .collect();
    let out = ProbeStore {
        cert_fp: store.cert_fp.clone(),
        results: persisted,
    };
    if let Ok(text) = serde_json::to_string_pretty(&out) {
        std::fs::write(path.with_extension("tmp"), text).ok();
        let _ = std::fs::rename(path.with_extension("tmp"), path);
    }
}

/// ConnAck 非成功码 → 中文原因
fn reject_reason(code: &ConnectReturnCode) -> String {
    match code {
        ConnectReturnCode::BadUserNamePassword => "用户/密码错误（SAS 凭据被拒）".to_string(),
        ConnectReturnCode::NotAuthorized => {
            "未授权（证书/角色不被该服务器接受）".to_string()
        }
        ConnectReturnCode::ProtocolError => "协议错误".to_string(),
        ConnectReturnCode::ClientIdentifierNotValid => "客户端 ID 无效".to_string(),
        other => format!("服务器拒绝连接（{other:?}）"),
    }
}

/// 单次连接尝试的结果
enum Attempt {
    Ok,
    /// 认证类拒绝（值得换角色重试）
    Rejected(String),
    /// 网络/协议层失败（换角色无意义）
    Failed(String),
}

/// 独立 MQTT 会话尝试登录一次：成功返回 Ok，ConnAck 非成功按码分类。
async fn try_login_once(
    host: &str,
    port: u16,
    tls: bool,
    client_id: &str,
    username: &str,
    password: &str,
) -> Attempt {
    // TCP 预检：DNS + 连通性，失败快速给出可定位原因
    let addr_str = format!("{host}:{port}");
    match tokio::net::lookup_host(addr_str.clone()).await {
        Ok(mut addrs) => {
            let mut connected = false;
            let mut first_err: Option<String> = None;
            while let Some(addr) = addrs.next() {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(TCP_TIMEOUT_S),
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
                return Attempt::Failed(format!(
                    "TCP 无法连接 {addr_str}（{}）",
                    first_err.unwrap_or_else(|| "no address".into())
                ));
            }
        }
        Err(e) => return Attempt::Failed(format!("DNS 解析失败 {addr_str}: {e}")),
    }

    let mut opts = MqttOptions::new(client_id.to_string(), host.to_string(), port);
    opts.set_keep_alive(std::time::Duration::from_secs(30));
    opts.set_credentials(username.to_string(), password.to_string());
    if tls {
        opts.set_transport(rumqttc::Transport::tls_with_default_config());
    }
    let (client, mut eventloop) = AsyncClient::new(opts, 10);
    let deadline = std::time::Duration::from_secs(CONNACK_TIMEOUT_S);
    let outcome = loop {
        match tokio::time::timeout(deadline, eventloop.poll()).await {
            Ok(Ok(Event::Incoming(Packet::ConnAck(ack)))) => {
                if ack.code == ConnectReturnCode::Success {
                    break Attempt::Ok;
                }
                let reason = reject_reason(&ack.code);
                if matches!(
                    ack.code,
                    ConnectReturnCode::BadUserNamePassword | ConnectReturnCode::NotAuthorized
                ) {
                    break Attempt::Rejected(reason);
                }
                break Attempt::Failed(reason);
            }
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => break Attempt::Failed(format!("连接失败：{e}")),
            Err(_) => {
                break Attempt::Failed(format!(
                    "MQTT 握手超时（{CONNACK_TIMEOUT_S}s 未收到 ConnAck）"
                ))
            }
        }
    };
    let _ = client.disconnect().await;
    outcome
}

/// 对一台服务器做完整探测（含角色轮换）。返回 Ok 或失败原因。
async fn probe_one(certs_dir: &Path, entry: &serde_json::Value) -> Result<(), String> {
    let host = entry
        .get("host")
        .and_then(|h| h.as_str())
        .unwrap_or("")
        .to_string();
    let port = entry.get("port").and_then(|p| p.as_u64()).unwrap_or(1883) as u16;
    if host.is_empty() || port == 0 {
        return Err("服务器缺少地址/端口".into());
    }
    let cert = entry.get("cert").cloned().unwrap_or(serde_json::Value::Null);
    let uid = cert.get("uid").and_then(|u| u.as_u64());
    if !cert.is_object() || uid.is_none() {
        return Err("STATION 广播缺少服务器证书，无法构建登录凭据".into());
    }
    let srv = json!({
        "host": host,
        "port": port,
        "callsign": cert.get("callsign").cloned().unwrap_or(json!("")),
        "uid": cert.get("uid").cloned(),
        "fingerprint": fmo_auth::beacon_cert_fingerprint(&cert),
    });
    // 本机证书问题（过期/私钥不配套）时所有探测同因失败，直接透出
    fmo_auth::validate_identity(certs_dir)?;

    let mut role = fmo_auth::initial_role(certs_dir, &srv);
    if !ROLE_SEQ.contains(&role.as_str()) {
        role = ROLE_SEQ[0].to_string();
    }
    let mut remaining: Vec<&'static str> =
        ROLE_SEQ.iter().copied().filter(|r| *r != role).collect();
    // 探测 client id 与主连接区分（P 前缀后缀随机），互不踢线
    let ident = fmo_auth::load_identity(certs_dir)?;
    let username = ident["user_cert"]["subject"]["callsign"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let my_uid = ident["user_cert"]["subject"]["uid"]
        .as_u64()
        .unwrap_or(0) as u32;
    let mut sfx = [0u8; 2];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut sfx);
    let cid = crate::fmo::mqtt_client::client_id_for(
        &username,
        my_uid,
        &format!("P{}", hex::encode_upper(sfx)),
    );
    let tls = port == 8883;

    let mut last_reject = String::new();
    loop {
        let creds = fmo_auth::mqtt_credentials(certs_dir, &srv, &role)?;
        let u = creds["username"].as_str().unwrap_or("").to_string();
        let p = creds["password"].as_str().unwrap_or("").to_string();
        match try_login_once(&host, port, tls, &cid, &u, &p).await {
            Attempt::Ok => return Ok(()),
            Attempt::Failed(e) => return Err(e),
            Attempt::Rejected(e) => {
                last_reject = e;
                if remaining.is_empty() {
                    return Err(format!(
                        "认证被拒（已轮换全部角色 user/super/admin）：{last_reject}"
                    ));
                }
                role = remaining.remove(0).to_string();
            }
        }
    }
}

impl ProbeEngine {
    pub fn new(
        emit: EmitFn,
        data_dir: &Path,
        table: Arc<ServerTable>,
        mqtt_state: Arc<Mutex<String>>,
        selected_server: Arc<Mutex<serde_json::Value>>,
    ) -> Self {
        let certs_dir = data_dir.join("certs");
        let persist_path = data_dir.join("probe_results.json");
        let mut store = load_store(&persist_path);
        // 证书已更换（或无证书）→ 旧结果全部作废，重新验证
        let cur_fp = current_cert_fp(&certs_dir);
        if store.cert_fp != cur_fp {
            store.results.clear();
            store.cert_fp = cur_fp;
        }
        Self {
            emit,
            certs_dir,
            table,
            mqtt_state,
            selected_server,
            persist_path,
            store: Arc::new(Mutex::new(store)),
            queued: Arc::new(std::sync::Mutex::new(HashSet::new())),
            sem: Arc::new(Semaphore::new(PROBE_CONCURRENCY)),
            started: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 探测结果全量（快照/事件用）
    pub async fn results(&self) -> HashMap<String, ProbeRecord> {
        self.store.lock().await.results.clone()
    }

    fn emit_results(&self, store: &ProbeStore) {
        (self.emit)(json!({"type": "probe_results", "probes": store.results}));
    }

    /// 当前已登录服务器的 key（host:port）；未连接为 None
    async fn connected_key(&self) -> Option<String> {
        if *self.mqtt_state.lock().await != "connected" {
            return None;
        }
        let sel = self.selected_server.lock().await;
        let host = sel.get("host").and_then(|h| h.as_str()).unwrap_or("");
        if host.is_empty() {
            return None;
        }
        let port = sel.get("port").and_then(|p| p.as_u64()).unwrap_or(1883);
        Some(format!("{host}:{port}"))
    }

    /// 入队一台服务器探测（已在队列/已有结果时不重复）。
    /// force=true（手动复验）时无视已有结果，但当前登录的服务器始终拒绝。
    pub async fn enqueue(&self, entry: serde_json::Value, force: bool) -> Result<(), String> {
        let host = entry
            .get("host")
            .and_then(|h| h.as_str())
            .unwrap_or("")
            .to_string();
        let port = entry.get("port").and_then(|p| p.as_u64()).unwrap_or(1883);
        let key = format!("{host}:{port}");
        if host.is_empty() {
            return Err("服务器缺少地址".into());
        }
        if self.connected_key().await.as_deref() == Some(key.as_str()) {
            if force {
                return Err("该服务器是当前登录的服务器，无需探测".into());
            }
            self.mark_connected_ok(&key).await;
            return Ok(());
        }
        {
            let mut queued = self.queued.lock().unwrap();
            if !queued.insert(key.clone()) {
                return Ok(()); // 已在队列
            }
        }
        if !force {
            let store = self.store.lock().await;
            if store.results.contains_key(&key) {
                self.queued.lock().unwrap().remove(&key);
                return Ok(());
            }
        }
        // 标记 probing 并广播
        {
            let mut store = self.store.lock().await;
            store.results.insert(
                key.clone(),
                ProbeRecord {
                    status: "probing".into(),
                    error: String::new(),
                    ts: chrono::Utc::now().timestamp(),
                },
            );
            self.emit_results(&store);
        }
        let this = self.clone_handle();
        tauri::async_runtime::spawn(async move {
            let _permit = this.sem.clone().acquire_owned().await.unwrap();
            let outcome = probe_one(&this.certs_dir, &entry).await;
            let mut store = this.store.lock().await;
            let rec = match &outcome {
                Ok(()) => ProbeRecord {
                    status: "ok".into(),
                    error: String::new(),
                    ts: chrono::Utc::now().timestamp(),
                },
                Err(e) => ProbeRecord {
                    status: "fail".into(),
                    error: e.clone(),
                    ts: chrono::Utc::now().timestamp(),
                },
            };
            if let Err(e) = &outcome {
                (this.emit)(json!({"type": "log", "level": "info",
                    "msg": format!("登录探测 {key}：{e}")}));
            } else {
                (this.emit)(json!({"type": "log", "level": "info",
                    "msg": format!("登录探测 {key}：可登录")}));
            }
            store.results.insert(key.clone(), rec);
            save_store(&this.persist_path, &store);
            this.emit_results(&store);
            drop(store);
            this.queued.lock().unwrap().remove(&key);
        });
        Ok(())
    }

    fn clone_handle(&self) -> Self {
        Self {
            emit: self.emit.clone(),
            certs_dir: self.certs_dir.clone(),
            table: self.table.clone(),
            mqtt_state: self.mqtt_state.clone(),
            selected_server: self.selected_server.clone(),
            persist_path: self.persist_path.clone(),
            store: self.store.clone(),
            queued: self.queued.clone(),
            sem: self.sem.clone(),
            started: self.started.clone(),
        }
    }

    /// 已连接服务器直接标记已验证（连接本身就是登录成功的证明）
    async fn mark_connected_ok(&self, key: &str) {
        let mut store = self.store.lock().await;
        let already = store
            .results
            .get(key)
            .map(|r| r.status == "ok")
            .unwrap_or(false);
        if !already {
            store.results.insert(
                key.to_string(),
                ProbeRecord {
                    status: "ok".into(),
                    error: String::new(),
                    ts: chrono::Utc::now().timestamp(),
                },
            );
            save_store(&self.persist_path, &store);
            self.emit_results(&store);
        }
    }

    /// 手动复验单台（清掉旧结果重新探测）
    pub async fn probe_manual(&self, key: &str) -> Result<(), String> {
        let entry = self
            .table
            .to_list()
            .await
            .into_iter()
            .find(|e| {
                let h = e.get("host").and_then(|h| h.as_str()).unwrap_or("");
                let p = e.get("port").and_then(|p| p.as_u64()).unwrap_or(1883);
                format!("{h}:{p}") == key
            })
            .ok_or_else(|| format!("服务器表里找不到 {key}（等它的 STATION 广播后重试）"))?;
        {
            let mut store = self.store.lock().await;
            store.results.remove(key);
            save_store(&self.persist_path, &store);
        }
        self.enqueue(entry, true).await
    }

    /// 手动复验全部失败项
    pub async fn probe_failed(&self) -> usize {
        let failed: Vec<String> = {
            let store = self.store.lock().await;
            store
                .results
                .iter()
                .filter(|(_, r)| r.status == "fail")
                .map(|(k, _)| k.clone())
                .collect()
        };
        let n = failed.len();
        for key in failed {
            let _ = self.probe_manual(&key).await;
        }
        n
    }

    /// 扫描一轮：证书指纹变化 → 清空重验；未验证/未入队的新服务器入队；
    /// 当前登录服务器标记已验证。
    async fn scan_once(&self) {
        let cur_fp = current_cert_fp(&self.certs_dir);
        if cur_fp.is_empty() {
            return; // 无证书不探测
        }
        {
            let mut store = self.store.lock().await;
            if store.cert_fp != cur_fp {
                store.results.clear();
                store.cert_fp = cur_fp;
                save_store(&self.persist_path, &store);
                self.emit_results(&store);
                (self.emit)(json!({"type": "log", "level": "info",
                    "msg": "检测到证书已更换，服务器登录验证结果已重置，重新探测"}));
            }
        }
        if let Some(key) = self.connected_key().await {
            self.mark_connected_ok(&key).await;
        }
        for entry in self.table.to_list().await {
            let _ = self.enqueue(entry, false).await;
        }
    }

    /// 启动周期扫描（幂等）
    pub fn start(self: &Arc<Self>) {
        if self.started.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let this = self.clone_handle();
        tauri::async_runtime::spawn(async move {
            loop {
                this.scan_once().await;
                tokio::time::sleep(std::time::Duration::from_secs(SCAN_INTERVAL_S)).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_roundtrip_skips_probing() {
        let dir = std::env::temp_dir().join(format!("probe-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("probe_results.json");
        let mut store = ProbeStore::default();
        store.cert_fp = "abc".into();
        store.results.insert(
            "a:1883".into(),
            ProbeRecord { status: "ok".into(), error: String::new(), ts: 1 },
        );
        store.results.insert(
            "b:1883".into(),
            ProbeRecord { status: "fail".into(), error: "认证被拒".into(), ts: 2 },
        );
        store.results.insert(
            "c:1883".into(),
            ProbeRecord { status: "probing".into(), error: String::new(), ts: 3 },
        );
        save_store(&path, &store);
        let loaded = load_store(&path);
        assert_eq!(loaded.cert_fp, "abc");
        assert_eq!(loaded.results["a:1883"].status, "ok");
        assert_eq!(loaded.results["b:1883"].error, "认证被拒");
        assert!(!loaded.results.contains_key("c:1883"), "probing 不落盘");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reject_reasons_cover_auth_codes() {
        assert!(reject_reason(&ConnectReturnCode::BadUserNamePassword).contains("用户/密码"));
        assert!(reject_reason(&ConnectReturnCode::NotAuthorized).contains("未授权"));
        assert!(reject_reason(&ConnectReturnCode::ProtocolError).contains("协议"));
    }
}
