//! FMO 服务器广播（FMO-V4 STATION，APRS APFMO4 位置包）+ 个人信标（BEACON）。
//!
//! 个人信标（本文件后半部分）：BEACON（APFMO4，签名，10 分钟周期）+ APFMO2 个性化
//! 消息（BEACON 成功后跟发）+ APFMO1 登录公告（STATION send() 成功后跟发），
//! 协议均已实网实锤（9/9 实捕验签通过），门控见 BeaconEngine 注释。
//!
//! 协议逆向自原厂固件（fmo-sim/docs/firmware-analysis.md §8.3）：
//! - 报文：`CS>APFMO4,TCPIP*:=<位置>F<经度>iFMO-V4,STATION,CERT:<b64url CBOR>,<国家>,<UTF-8名称>,<host>,P<port>,F<覆盖>KM,U<在线>/<峰值>,SIG:<b64url 64B Ed25519>`
//! - 线上文本字段一律 UTF-8（官方文档 bg5esn.com/docs/fmo-aprs-formate/ 规定；
//!   地图服务器 map.fmo.net.cn 实测拒绝 GBK 报文、接受 UTF-8 报文；原厂新固件实捕
//!   也是 UTF-8）。与 TBS 签名内文本同字节，不再做 UTF-8→GBK 转换
//! - 周期：5/10/60 分钟可配（原厂默认 10 分钟）；手动广播有 60s 最小间隔（同原厂限速）
//! - CERT = 本机用户证书的 10 元素 CBOR；SIG = 设备私钥（cert_devicekey）对 TBS 的 Ed25519 签名
//!   （TBS 16 元素布局已实网验签实锤，见 build_station_tbs 注释）

use crate::fmo::aprs::{AprsTx, EmitFn};
use crate::fmo::presence::PresenceTracker;
use crate::fmo::protocol;
use crate::fmo::qso::base_call;
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

/// 手动/自动广播的最小间隔（原厂 sendV4Station 的 60s 速率限制）
const MIN_INTERVAL_S: i64 = 60;

/// 台站名称校验（与原版"自定义服务器"配置对齐）：最大 32 个 Unicode 字符
/// （32 个中文也要能放下）；英文逗号会破坏报文逗号分隔格式，禁止。
/// 线上文本为 UTF-8（官方文档规定 + 地图服务器实测拒绝 GBK），不再做 GBK
/// 可编码性检查；UTF-8 下任何 Unicode 文本（含 emoji）都合法。
fn validate_station_name(name: &str) -> Result<(), String> {
    if name.contains(',') {
        return Err("台站名称不能包含英文逗号（会破坏报文格式）".into());
    }
    let chars = name.chars().count();
    if chars > 32 {
        return Err(format!("台站名称最长 32 字符（当前 {chars} 字符）"));
    }
    Ok(())
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct BroadcastConfig {
    /// 自动广播周期（分钟）：0=关闭，5/10/60
    pub mode_min: u32,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub cover_km: u32,
    /// 在线数：0=自动（默认，取 LATE 心跳花名册统计值），>0=手动覆盖
    pub online: u32,
    /// 峰值：0=自动（默认，取本地累计历史峰值），>0=手动覆盖
    pub peak: u32,
    pub country: String,
    pub lat: f64,
    pub lon: f64,
    /// 广播呼号的 APRS SSID（0-15，0=不带）。与原版固件对齐：原版跟随
    /// APRS-IS 登录呼号的 SSID（如 BD4VKI-15）；包头和 TBS 签同一个值。
    /// 0 也合法，但个别地图服务器对 ssid=0 有额外过滤时可用此项对齐原版行为。
    #[serde(default)]
    pub ssid: u32,
    /// 梅登黑德网格（4/6 位）。留空 = 由经纬度自动推导（对齐原版固件：
    /// 原版网格是独立配置项，GRIDS 界面手填，未配置显示 GRID NOT SET）。
    /// PTT 时随成员 JSON（isSpeaking/grid）发布到 FMO/QSO/UID/<本机uid>，
    /// 对端据此显示距离/方位。
    #[serde(default)]
    pub grid: String,
}

impl Default for BroadcastConfig {
    fn default() -> Self {
        Self {
            mode_min: 0,
            name: String::new(),
            host: String::new(),
            port: 1883,
            cover_km: 100,
            online: 0,
            peak: 0,
            country: "CN".into(),
            lat: 39.9,
            lon: 116.4,
            ssid: 0,
            grid: String::new(),
        }
    }
}

impl BroadcastConfig {
    /// 有效网格：配置非空且合法（4/6 位梅登黑德）则用配置值（大写），
    /// 否则由经纬度推导 6 位网格。
    pub fn effective_grid(&self) -> String {
        let g = self.grid.trim().to_uppercase();
        if crate::fmo::qso::grid_to_latlon(&g).is_some() {
            return g;
        }
        crate::fmo::qso::maidenhead_grid(self.lat, self.lon)
    }
}

pub struct BroadcastEngine {
    emit: EmitFn,
    tx: Arc<AprsTx>,
    cfg: Arc<Mutex<BroadcastConfig>>,
    cfg_path: PathBuf,
    last_sent: Arc<Mutex<i64>>,
    started: Arc<AtomicBool>,
    data_dir: PathBuf,
    /// 广播 super 门控所需的运行时引用（由 state 注入）
    mqtt_state: Arc<Mutex<String>>,
    mqtt_role: Arc<Mutex<String>>,
    selected_server: Arc<Mutex<serde_json::Value>>,
    /// 在线数/峰值自动统计花名册（LATE 心跳），由 state 注入
    presence: Arc<PresenceTracker>,
    /// 个人信标引擎（STATION send() 成功后跟发 APFMO1 登录公告），由 state 注入
    beacon: std::sync::Mutex<Option<Arc<BeaconEngine>>>,
}

/// online/peak 生效值判定（纯函数，便于测试）：0=自动，>0=手动覆盖。
fn effective_value(manual: u32, auto: u32) -> u32 {
    if manual > 0 {
        manual
    } else {
        auto
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn aprs_lat(lat: f64) -> String {
    let hemi = if lat >= 0.0 { 'N' } else { 'S' };
    let a = lat.abs();
    let d = a.trunc() as u32;
    let m = (a - d as f64) * 60.0;
    format!("{d:02}{m:05.2}{hemi}")
}

fn aprs_lon(lon: f64) -> String {
    let hemi = if lon >= 0.0 { 'E' } else { 'W' };
    let a = lon.abs();
    let d = a.trunc() as u32;
    let m = (a - d as f64) * 60.0;
    format!("{d:03}{m:05.2}{hemi}")
}

/// STATION 广播的 SIG 待签名数据（TBS）。
///
/// 实网实锤布局（30/30 台 STATION 广播验签 100% 通过，与官方文档
/// bg5esn.com/docs/fmo-aprs-formate/ §8.3 一致；固件 TBS 构造 VMA 0x4202ccac）：
/// 16 元素确定性 CBOR 数组：
///   0 "FMO" | 1 4 | 2 "STATION" | 3 呼号大写（不含 SSID）| 4 SSID(uint，无 SSID=0)
///   5 纬度串 "3952.80N" | 6 经度串 "11931.57E"（与位置前缀同字符串）
///   7 SHA-256(完整 CERT CBOR blob) 32B | 8 国家码大写 | 9 名称（UTF-8，与线上同字节）
///   10 host | 11 port | 12 覆盖km | 13 在线 | 14 峰值 | 15 time(NULL)/600（10 分钟槽）
#[allow(clippy::too_many_arguments)]
fn build_station_tbs(
    callsign: &str,
    ssid: u32,
    lat_str: &str,
    lon_str: &str,
    cert_blob_hash: &[u8],
    country: &str,
    name: &str,
    host: &str,
    port: u16,
    cover_km: u32,
    online: u32,
    peak: u32,
    ts_slot: u64,
) -> Vec<u8> {
    use protocol::CborValue as C;
    protocol::cbor_tbs(&[
        C::Text("FMO".into()),
        C::UInt(4),
        C::Text("STATION".into()),
        C::Text(callsign.to_uppercase()),
        C::UInt(ssid as u64),
        C::Text(lat_str.to_string()),
        C::Text(lon_str.to_string()),
        C::Bytes(cert_blob_hash.to_vec()),
        C::Text(country.to_uppercase()),
        C::Text(name.to_string()),
        C::Text(host.to_string()),
        C::UInt(port as u64),
        C::UInt(cover_km as u64),
        C::UInt(online as u64),
        C::UInt(peak as u64),
        C::UInt(ts_slot),
    ])
}

/// 广播 super 门控判定（纯函数，便于测试）：
/// 仅当 MQTT 已连接、最终认证角色 == "super"、且选定服务器呼号 == 本机证书呼号
/// （即登录的是自己服务器）时放行。呼号比较剥离 SSID（服务器列表里的
/// "BG9JYT-14" 与证书基础呼号 "BG9JYT" 应视为同一台）。
/// 注意：admin 暂不等同 super —— 原厂是否允许 admin 开广播尚未实网验证，先从严。
fn gate_decide(mqtt_state: &str, role: &str, server_cs: &str, cert_cs: &str) -> Result<(), String> {
    if mqtt_state != "connected" {
        return Err("MQTT 未连接：服务器广播需要先连接自己的服务器".into());
    }
    if role != "super" {
        let shown = if role.is_empty() { "未知" } else { role };
        return Err(format!(
            "当前 MQTT 角色为 {shown}，服务器广播需以 super 身份登录自己的服务器"
        ));
    }
    let server_base = base_call(server_cs);
    let cert_base = base_call(cert_cs);
    if server_base.is_empty() || cert_base.is_empty() || server_base != cert_base {
        let shown = if server_base.is_empty() {
            "未知".to_string()
        } else {
            server_base
        };
        return Err(format!(
            "当前登录的不是自己的服务器（服务器呼号 {shown} ≠ 本机证书呼号 {cert_base}），不能广播",
        ));
    }
    Ok(())
}

impl BroadcastEngine {
    pub fn new(
        emit: EmitFn,
        tx: Arc<AprsTx>,
        data_dir: PathBuf,
        mqtt_state: Arc<Mutex<String>>,
        mqtt_role: Arc<Mutex<String>>,
        selected_server: Arc<Mutex<serde_json::Value>>,
        presence: Arc<PresenceTracker>,
    ) -> Self {
        let cfg_path = data_dir.join("broadcast.json");
        let cfg: BroadcastConfig = std::fs::read_to_string(&cfg_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Self {
            emit,
            tx,
            cfg: Arc::new(Mutex::new(cfg)),
            cfg_path,
            last_sent: Arc::new(Mutex::new(0)),
            started: Arc::new(AtomicBool::new(false)),
            data_dir,
            mqtt_state,
            mqtt_role,
            selected_server,
            presence,
            beacon: std::sync::Mutex::new(None),
        }
    }

    /// 注入个人信标引擎（APFMO1 登录公告跟发用）
    pub fn set_beacon(&self, beacon: Arc<BeaconEngine>) {
        *self.beacon.lock().unwrap() = Some(beacon);
    }

    pub async fn config(&self) -> BroadcastConfig {
        self.cfg.lock().await.clone()
    }

    /// online/peak 生效值：配置 0=自动（花名册统计），>0=手动覆盖。
    /// 供组包与 stats_snapshot 使用（自动在线数同时驱动峰值累计）。
    pub fn effective_online_peak(&self, cfg: &BroadcastConfig) -> (u32, u32) {
        (
            effective_value(cfg.online, self.presence.online()),
            effective_value(cfg.peak, self.presence.peak()),
        )
    }

    /// 保存广播配置。开启自动广播（mode_min>0）时先做 super 门控，
    /// 不满足条件拒绝保存并说明原因；关闭（mode_min=0）总是允许。
    pub async fn set_config(&self, cfg: BroadcastConfig) -> Result<(), String> {
        validate_station_name(&cfg.name)?;
        if cfg.mode_min > 0 {
            self.gate_check()
                .await
                .map_err(|e| format!("无法开启自动广播：{e}"))?;
        }
        {
            let mut guard = self.cfg.lock().await;
            *guard = cfg;
        }
        let guard = self.cfg.lock().await;
        if let Ok(text) = serde_json::to_string_pretty(&*guard) {
            std::fs::write(&self.cfg_path, text).ok();
        }
        Ok(())
    }

    /// super 门控：收集运行时状态（MQTT 状态/最终角色/选定服务器呼号/本机证书呼号）
    /// 后交给 gate_decide 判定。
    async fn gate_check(&self) -> Result<(), String> {
        let mqtt_state = self.mqtt_state.lock().await.clone();
        let role = self.mqtt_role.lock().await.clone();
        // 选定服务器呼号：优先顶层 callsign，缺失时回退到 STATION 广播证书里的呼号
        let server_cs = {
            let sel = self.selected_server.lock().await;
            sel.get("callsign")
                .and_then(|c| c.as_str())
                .or_else(|| {
                    sel.get("cert")
                        .and_then(|c| c.get("callsign"))
                        .and_then(|c| c.as_str())
                })
                .unwrap_or("")
                .to_string()
        };
        let cert_cs = self.cert_blob()?.0;
        gate_decide(&mqtt_state, &role, &server_cs, &cert_cs)
    }

    /// 广播资格查询（供 fmo_broadcast_eligible 命令）：(可否广播, 原因, 当前角色)
    pub async fn eligible(&self) -> (bool, String, String) {
        let role = self.mqtt_role.lock().await.clone();
        match self.gate_check().await {
            Ok(()) => (true, String::new(), role),
            Err(e) => (false, e, role),
        }
    }

    /// 本机用户证书 → (呼号, CERT blob 原始字节, b64url(blob), 设备私钥 seed)
    fn cert_blob(&self) -> Result<(String, Vec<u8>, String, String), String> {
        cert_blob(&self.data_dir)
    }

    /// 广播配置共享句柄（BEACON 引擎读取经纬度等位置字段用）
    pub fn cfg_handle(&self) -> Arc<Mutex<BroadcastConfig>> {
        self.cfg.clone()
    }
}

/// 本机用户证书 → (呼号, CERT blob 原始字节, b64url(blob), 设备私钥 seed)
/// （STATION 广播与个人信标 BEACON 共用同一份用户证书）
fn cert_blob(data_dir: &std::path::Path) -> Result<(String, Vec<u8>, String, String), String> {
    {
        let certs = data_dir.join("certs");
        let text = std::fs::read_to_string(certs.join("cert_user.json"))
            .map_err(|e| format!("读取 cert_user.json 失败：{e}"))?;
        let uc: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("cert_user.json 解析失败：{e}"))?;
        let callsign = uc["subject"]["callsign"]
            .as_str()
            .ok_or("证书缺少呼号")?
            .to_string();
        let uid = uc["subject"]["uid"].as_u64().ok_or("证书缺少 uid")?;
        let pubkey = protocol::decode_seed(uc["subject"]["publicKey"].as_str().unwrap_or(""))
            .ok_or("证书公钥解码失败")?;
        let iat = uc["iat"].as_u64().ok_or("证书缺少 iat")?;
        let exp = uc["exp"].as_u64().ok_or("证书缺少 exp")?;
        let issuer_sn = uc["issuerSn"].as_u64().ok_or("证书缺少 issuerSn")?;
        let ca_sig = protocol::decode_seed(uc["signature"].as_str().unwrap_or(""))
            .ok_or("证书 CA 签名缺失/无法解码")?;
        let arr = vec![
            protocol::CborValue::Text("FMO".into()),
            protocol::CborValue::UInt(4),
            protocol::CborValue::Text("userCert".into()),
            protocol::CborValue::UInt(issuer_sn),
            protocol::CborValue::Text(callsign.clone()),
            protocol::CborValue::UInt(uid),
            protocol::CborValue::Bytes(pubkey),
            protocol::CborValue::UInt(iat),
            protocol::CborValue::UInt(exp),
            protocol::CborValue::Bytes(ca_sig),
        ];
        let blob = protocol::cbor_tbs(&arr);
        let b64 = protocol::b64url_encode(&blob);
        // 设备私钥 seed（cert_devicekey.json），用于 SIG 签名
        let dk_text = std::fs::read_to_string(certs.join("cert_devicekey.json"))
            .map_err(|e| format!("读取 cert_devicekey.json 失败：{e}"))?;
        let dk: serde_json::Value = serde_json::from_str(&dk_text)
            .map_err(|e| format!("cert_devicekey.json 解析失败：{e}"))?;
        let seed = dk["seed"].as_str().unwrap_or("").to_string();
        if seed.is_empty() {
            return Err("cert_devicekey.json 缺少 seed".into());
        }
        Ok((callsign, blob, b64, seed))
    }
}

impl BroadcastEngine {
    /// 构造完整 STATION 广播报文（不含换行）。online/peak 用生效值（0=自动统计，>0=手动覆盖）。
    /// 返回（报文字节，广播源呼号）；源呼号同时用于 APFMO1 登录公告跟发。
    fn build_packet(&self, cfg: &BroadcastConfig) -> Result<(Vec<u8>, String), String> {
        let (callsign, cert_blob, cert_b64, seed) = self.cert_blob()?;
        let (online, peak) = self.effective_online_peak(cfg);
        let lat_str = aprs_lat(cfg.lat);
        let lon_str = aprs_lon(cfg.lon);
        // 广播源呼号：证书基础呼号 + 配置的 SSID（0=不带，对齐原版跟随
        // APRS 登录呼号的行为）；包头 SSID 与 TBS 第 4 元素必须同值。
        let ssid = cfg.ssid.min(15);
        let source = source_callsign(&callsign, ssid);
        // SIG：TBS 第 7 元素 = SHA-256(完整 CERT blob)，名称 UTF-8（与线上同字节）
        let cert_hash = {
            use sha2::Digest;
            let mut h = sha2::Sha256::new();
            h.update(&cert_blob);
            h.finalize().to_vec()
        };
        let ts_slot = (now() / 600) as u64;
        let tbs = build_station_tbs(
            &callsign,
            ssid,
            &lat_str,
            &lon_str,
            &cert_hash,
            &cfg.country,
            &cfg.name,
            &cfg.host,
            cfg.port,
            cfg.cover_km,
            online,
            peak,
            ts_slot,
        );
        let sig = protocol::sign(&seed, &tbs)?;
        let mut line = format!(
            "{cs}>APFMO4,TCPIP*:={lat}F{lon}iFMO-V4,STATION,CERT:{cert},{country},",
            cs = source,
            lat = lat_str,
            lon = lon_str,
            cert = cert_b64,
            country = cfg.country,
        )
        .into_bytes();
        line.extend_from_slice(cfg.name.as_bytes());
        line.extend_from_slice(
            format!(
                ",{host},P{port},F{cov}KM,U{on}/{peak},SIG:{sig}",
                host = cfg.host,
                port = cfg.port,
                cov = cfg.cover_km,
                on = online,
                peak = peak,
                sig = protocol::b64url_encode(&sig),
            )
            .as_bytes(),
        );
        Ok((line, source))
    }

    /// 发送一次广播。manual=true 时 60s 内重复点击报错；自动到点发送静默跳过。
    pub async fn send(&self, manual: bool) -> Result<(), String> {
        let t = now();
        {
            let last = *self.last_sent.lock().await;
            if t - last < MIN_INTERVAL_S {
                if manual {
                    return Err(format!(
                        "距上次广播不足 {}s，稍后再试",
                        MIN_INTERVAL_S - (t - last)
                    ));
                }
                return Ok(());
            }
        }
        let cfg = self.cfg.lock().await.clone();
        if cfg.host.trim().is_empty() || cfg.port == 0 {
            return Err("请先在 FMO 设置里填写服务器广播配置（名称/地址/端口）".into());
        }
        // super 门控：仅自己服务器 + super 角色可广播；手动发送把原因返回给前端，
        // 自动循环由 start() 把该 Err 记为 warn 日志后跳过
        self.gate_check().await?;
        self.tx.gate_verified().await?;
        let (line, source) = self.build_packet(&cfg)?;
        let (online, peak) = self.effective_online_peak(&cfg);
        let preview = String::from_utf8_lossy(&line).into_owned();
        self.tx.send_packet(line).await?;
        *self.last_sent.lock().await = t;
        (self.emit)(json!({"type": "log", "level": "info",
            "msg": format!("服务器广播已发送（{}:{} 在线{}/{}）：{}",
                           cfg.host, cfg.port, online, peak, preview)}));
        (self.emit)(json!({"type": "broadcast_state", "lastSent": t}));
        // APFMO1 登录公告跟发（与原厂一致：STATION 广播成功后跟发，公告为空则不发）
        let beacon = self.beacon.lock().unwrap().clone();
        if let Some(beacon) = beacon {
            beacon.follow_announcement(&source, &cfg.name, online, peak).await;
        }
        Ok(())
    }

    /// 自动广播循环：15s 检查一次，到点即发（last_sent=0 时启用后立即首播）。
    pub fn start(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::Relaxed) {
            return;
        }
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                let (mode_min, due) = {
                    let cfg = this.cfg.lock().await;
                    let last = *this.last_sent.lock().await;
                    let due = cfg.mode_min > 0 && now() - last >= (cfg.mode_min as i64) * 60;
                    (cfg.mode_min, due)
                };
                if mode_min > 0 && due {
                    if let Err(e) = this.send(false).await {
                        (this.emit)(json!({"type": "log", "level": "warn",
                            "msg": format!("自动服务器广播失败：{e}")}));
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            }
        });
    }
}

// ---------------------------------------------------------------- 个人信标（BEACON）

/// BEACON 整条帧上限（官方限制，超长拒绝发送）
const BEACON_MAX_LEN: usize = 512;
/// BEACON 固定周期 10 分钟（原厂 buildV4Beacon 重排间隔，不可配）
const BEACON_PERIOD_S: i64 = 600;

/// 广播源呼号：基础呼号 + SSID（0=不带，>15 截断）。包头与 TBS 签同一个值。
fn source_callsign(callsign: &str, ssid: u32) -> String {
    let ssid = ssid.min(15);
    if ssid > 0 {
        format!("{callsign}-{ssid}")
    } else {
        callsign.to_string()
    }
}

/// BEACON 文本字段校验：禁英文逗号（破坏报文逗号分隔）、字符数超限拒绝。
/// 线上文本为 UTF-8，不再做 GBK 可编码性检查。
fn validate_beacon_field(label: &str, value: &str, max_chars: usize) -> Result<(), String> {
    if value.contains(',') {
        return Err(format!("{label}不能包含英文逗号（会破坏报文格式）"));
    }
    let chars = value.chars().count();
    if chars > max_chars {
        return Err(format!("{label}最长 {max_chars} 字符（当前 {chars} 字符）"));
    }
    Ok(())
}

/// 个人信标配置（beacon.json，serde(default) 全字段兜底：旧文件缺字段也能反序列化）
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct BeaconConfig {
    /// 周期信标开关（固定 10 分钟周期 + 60s 限速）
    pub enabled: bool,
    /// 信标呼号的 APRS SSID（0-15，0=不带）
    pub ssid: u32,
    /// 电台名称（≤16 字符，线上 UTF-8 / TBS 内同字节）
    pub rig: String,
    /// 直频频率 MHz（>0 才发送；合法范围 20-500）
    pub freq_mhz: f64,
    /// 天线型号（≤16 字符）
    pub ant: String,
    /// 天线高度 m（0=报文中省略 HEIGHT 段）
    pub height_m: u32,
    /// APRS 个性化消息（≤64 字符，BEACON 发送成功后以 APFMO2 跟发）
    pub aprs_msg: String,
    /// 登录公告（≤128 字符，STATION 广播成功后以 APFMO1 跟发）
    pub notice: String,
    /// QSO 祝福语（≤128 字符）：QSO 建立时随完整通联记录 JSON 发布到
    /// FMO/QSO/UID/<对方uid>（toComment 字段），见 state.rs install_qso_wish_hooks
    pub qso_msg: String,
}

impl BeaconConfig {
    pub fn validate(&self) -> Result<(), String> {
        validate_beacon_field("电台名称", &self.rig, 16)?;
        validate_beacon_field("天线型号", &self.ant, 16)?;
        validate_beacon_field("APRS 个性化消息", &self.aprs_msg, 64)?;
        validate_beacon_field("登录公告", &self.notice, 128)?;
        // qso_msg 不进报文（仅存储），不限制逗号，只卡长度
        if self.qso_msg.chars().count() > 128 {
            return Err(format!(
                "QSO 祝福最长 128 字符（当前 {} 字符）",
                self.qso_msg.chars().count()
            ));
        }
        if self.freq_mhz != 0.0 && !(20.0..=500.0).contains(&self.freq_mhz) {
            return Err(format!(
                "直频频率需在 20-500 MHz 之间（当前 {}）",
                self.freq_mhz
            ));
        }
        Ok(())
    }
}

/// BEACON 的 SIG 待签名数据（TBS）。
///
/// 实网实锤布局（官方文档 + 9/9 实捕验签通过）：
/// 10-13 元素确定性 CBOR 数组，可选元素严格"省略"（不放进数组，不是置空）：
///   0 "FMO" | 1 4 | 2 "BEACON" | 3 呼号大写（不含 SSID）| 4 SSID(uint)
///   5 纬度串 | 6 经度串（与位置前缀同字符串）
///   7 SHA-256(完整 CERT CBOR blob) 32B | 8 freq 文本（"%.4f"）
///   [9 高度整数，仅 >0 才加] [10 电台 UTF-8，仅非空才加] [11 天线 UTF-8，仅非空才加]
///   末位 time 槽（ts/600，同 STATION 的 10 分钟槽约定）
#[allow(clippy::too_many_arguments)]
fn build_beacon_tbs(
    callsign: &str,
    ssid: u32,
    lat_str: &str,
    lon_str: &str,
    cert_blob_hash: &[u8],
    freq_str: &str,
    height_m: u32,
    rig: &str,
    ant: &str,
    ts_slot: u64,
) -> Vec<u8> {
    use protocol::CborValue as C;
    let mut arr = vec![
        C::Text("FMO".into()),
        C::UInt(4),
        C::Text("BEACON".into()),
        C::Text(callsign.to_uppercase()),
        C::UInt(ssid as u64),
        C::Text(lat_str.to_string()),
        C::Text(lon_str.to_string()),
        C::Bytes(cert_blob_hash.to_vec()),
        C::Text(freq_str.to_string()),
    ];
    if height_m > 0 {
        arr.push(C::UInt(height_m as u64));
    }
    if !rig.is_empty() {
        arr.push(C::Text(rig.to_string()));
    }
    if !ant.is_empty() {
        arr.push(C::Text(ant.to_string()));
    }
    arr.push(C::UInt(ts_slot));
    protocol::cbor_tbs(&arr)
}

/// 构造完整 BEACON 报文（不含换行）。RIG/ANT 线上 UTF-8（与 TBS 内同字节，
/// 同 STATION 名称模式）。整条帧 ≤512 字节（官方限制），超长拒绝发送。
fn build_beacon_frame(
    source: &str,
    lat_str: &str,
    lon_str: &str,
    cert_b64: &str,
    cfg: &BeaconConfig,
    sig_b64: &str,
) -> Result<Vec<u8>, String> {
    let mut line = format!(
        "{source}>APFMO4,TCPIP*:={lat}F{lon}iFMO-V4,BEACON,CERT:{cert},FREQ:{freq:.4}",
        lat = lat_str,
        lon = lon_str,
        cert = cert_b64,
        freq = cfg.freq_mhz,
    )
    .into_bytes();
    if cfg.height_m > 0 {
        line.extend_from_slice(format!(",HEIGHT:{}", cfg.height_m).as_bytes());
    }
    if !cfg.rig.is_empty() {
        line.extend_from_slice(b",RIG:");
        line.extend_from_slice(cfg.rig.as_bytes());
    }
    if !cfg.ant.is_empty() {
        line.extend_from_slice(b",ANT:");
        line.extend_from_slice(cfg.ant.as_bytes());
    }
    line.extend_from_slice(format!(",SIG:{sig_b64}").as_bytes());
    if line.len() > BEACON_MAX_LEN {
        return Err(format!(
            "BEACON 报文 {} 字节，超过 {BEACON_MAX_LEN} 字节上限（官方限制），请缩短电台/天线等字段",
            line.len()
        ));
    }
    Ok(line)
}

/// APFMO2 个性化消息（无签名）：CALL[-SSID]>APFMO2,TCPIP*:><UTF-8文本>
fn build_apfmo2_frame(source: &str, msg: &str) -> Vec<u8> {
    let mut line = format!("{source}>APFMO2,TCPIP*:>").into_bytes();
    line.extend_from_slice(msg.as_bytes());
    line
}

/// APFMO1 登录公告（无签名）：
/// CALL[-SSID]>APFMO1,TCPIP*:><名称UTF-8>,正常,在线/峰值:<X>/<Y>[,<公告UTF-8>]
/// （公告为空时省略最后一段，连逗号一起省略）
fn build_apfmo1_frame(source: &str, name: &str, online: u32, peak: u32, notice: &str) -> Vec<u8> {
    let mut line = format!("{source}>APFMO1,TCPIP*:>").into_bytes();
    line.extend_from_slice(name.as_bytes());
    line.extend_from_slice(format!(",正常,在线/峰值:{online}/{peak}").as_bytes());
    if !notice.is_empty() {
        line.push(b',');
        line.extend_from_slice(notice.as_bytes());
    }
    line
}

/// 个人信标引擎（APRS APFMO4 BEACON + APFMO2 个性化消息跟发）。
///
/// 门控与原厂一致（firmware-analysis §8.6）：APRS 上行 verified + 证书就绪 + freq>0，
/// 与 MQTT/服务器连接状态完全无耦合（不做 super 门控，普通证书用户即可用）。
pub struct BeaconEngine {
    emit: EmitFn,
    tx: Arc<AprsTx>,
    cfg: Arc<Mutex<BeaconConfig>>,
    cfg_path: PathBuf,
    last_sent: Arc<Mutex<i64>>,
    started: Arc<AtomicBool>,
    data_dir: PathBuf,
    /// 信标位置来源：复用设置页已有的广播经纬度（STATION 与 BEACON 同一 QTH）
    broadcast_cfg: Arc<Mutex<BroadcastConfig>>,
}

impl BeaconEngine {
    pub fn new(
        emit: EmitFn,
        tx: Arc<AprsTx>,
        data_dir: PathBuf,
        broadcast_cfg: Arc<Mutex<BroadcastConfig>>,
    ) -> Self {
        let cfg_path = data_dir.join("beacon.json");
        let cfg: BeaconConfig = std::fs::read_to_string(&cfg_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Self {
            emit,
            tx,
            cfg: Arc::new(Mutex::new(cfg)),
            cfg_path,
            last_sent: Arc::new(Mutex::new(0)),
            started: Arc::new(AtomicBool::new(false)),
            data_dir,
            broadcast_cfg,
        }
    }

    pub async fn config(&self) -> BeaconConfig {
        self.cfg.lock().await.clone()
    }

    pub async fn last_sent(&self) -> i64 {
        *self.last_sent.lock().await
    }

    /// 保存信标配置（仅字段校验；APRS verified/证书/freq 门控在发送时判定，
    /// 与原厂一致——不连服务器也可以先配置好）。
    pub async fn set_config(&self, cfg: BeaconConfig) -> Result<(), String> {
        cfg.validate()?;
        {
            let mut guard = self.cfg.lock().await;
            *guard = cfg;
        }
        let guard = self.cfg.lock().await;
        if let Ok(text) = serde_json::to_string_pretty(&*guard) {
            std::fs::write(&self.cfg_path, text).ok();
        }
        Ok(())
    }

    /// 发送一次信标。manual=true 时 60s 内重复点击报错；自动到点发送静默跳过。
    pub async fn send(&self, manual: bool) -> Result<(), String> {
        let t = now();
        {
            let last = *self.last_sent.lock().await;
            if t - last < MIN_INTERVAL_S {
                if manual {
                    return Err(format!(
                        "距上次信标不足 {}s，稍后再试",
                        MIN_INTERVAL_S - (t - last)
                    ));
                }
                return Ok(());
            }
        }
        let cfg = self.cfg.lock().await.clone();
        // 门控（与原厂一致）：APRS 上行 verified + 证书就绪 + freq>0；不做 super 门控
        self.tx.gate_verified().await?;
        let (callsign, cert_blob, cert_b64, seed) = cert_blob(&self.data_dir)?;
        if cfg.freq_mhz <= 0.0 {
            return Err("未配置直频频率（freq=0 时原厂当轮跳过信标）".into());
        }
        let (lat, lon) = {
            let bc = self.broadcast_cfg.lock().await;
            (bc.lat, bc.lon)
        };
        let lat_str = aprs_lat(lat);
        let lon_str = aprs_lon(lon);
        let ssid = cfg.ssid.min(15);
        let source = source_callsign(&callsign, ssid);
        let cert_hash = {
            use sha2::Digest;
            let mut h = sha2::Sha256::new();
            h.update(&cert_blob);
            h.finalize().to_vec()
        };
        let freq_str = format!("{:.4}", cfg.freq_mhz);
        let tbs = build_beacon_tbs(
            &callsign,
            ssid,
            &lat_str,
            &lon_str,
            &cert_hash,
            &freq_str,
            cfg.height_m,
            &cfg.rig,
            &cfg.ant,
            (t / 600) as u64,
        );
        let sig = protocol::sign(&seed, &tbs)?;
        let line = build_beacon_frame(
            &source,
            &lat_str,
            &lon_str,
            &cert_b64,
            &cfg,
            &protocol::b64url_encode(&sig),
        )?;
        let preview = String::from_utf8_lossy(&line).into_owned();
        self.tx.send_packet(line).await?;
        *self.last_sent.lock().await = t;
        (self.emit)(json!({"type": "log", "level": "info",
            "msg": format!("个人信标已发送（{:.4}MHz）：{}", cfg.freq_mhz, preview)}));
        (self.emit)(json!({"type": "beacon_state", "lastSent": t}));
        // APFMO2 个性化消息跟发（BEACON 成功后，aprs_msg 非空时；无签名，失败只记日志）
        if !cfg.aprs_msg.is_empty() {
            let frame = build_apfmo2_frame(&source, &cfg.aprs_msg);
            match self.tx.send_packet(frame).await {
                Ok(()) => (self.emit)(json!({"type": "log", "level": "info",
                    "msg": format!("APRS 个性化消息已发送：{}", cfg.aprs_msg)})),
                Err(e) => (self.emit)(json!({"type": "log", "level": "warn",
                    "msg": format!("APRS 个性化消息发送失败：{e}")})),
            }
        }
        Ok(())
    }

    /// APFMO1 登录公告跟发：STATION 广播 send() 成功路径调用（沿用 STATION 的
    /// super 门控，此处无需额外判定）；公告为空时不发。
    pub async fn follow_announcement(&self, source: &str, name: &str, online: u32, peak: u32) {
        let notice = self.cfg.lock().await.notice.clone();
        if notice.is_empty() {
            return;
        }
        let frame = build_apfmo1_frame(source, name, online, peak, &notice);
        match self.tx.send_packet(frame).await {
            Ok(()) => (self.emit)(json!({"type": "log", "level": "info",
                "msg": format!("登录公告已发送（在线{online}/{peak}）：{notice}")})),
            Err(e) => (self.emit)(json!({"type": "log", "level": "warn",
                "msg": format!("登录公告发送失败：{e}")})),
        }
    }

    /// 周期信标循环：15s 检查一次，enabled 且距上次 ≥10 分钟即发
    /// （last_sent=0 时启用后立即首播）。
    pub fn start(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::Relaxed) {
            return;
        }
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                let due = {
                    let cfg = this.cfg.lock().await;
                    let last = *this.last_sent.lock().await;
                    cfg.enabled && now() - last >= BEACON_PERIOD_S
                };
                if due {
                    if let Err(e) = this.send(false).await {
                        (this.emit)(json!({"type": "log", "level": "warn",
                            "msg": format!("自动个人信标失败：{e}")}));
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn station_name_validation() {
        // 空名、短名、32 个中文字符通过；线上 UTF-8，emoji 等任意 Unicode 也放行
        assert!(validate_station_name("").is_ok());
        assert!(validate_station_name("无锡FMO").is_ok());
        assert!(validate_station_name(&"中".repeat(32)).is_ok());
        assert!(validate_station_name("无锡FMO🎙").is_ok());
        // 33 字符拒绝
        assert!(validate_station_name(&"中".repeat(33)).is_err());
        assert!(validate_station_name(&"a".repeat(33)).is_err());
        // 含英文逗号拒绝
        assert!(validate_station_name("无锡,FMO").is_err());
    }

    #[test]
    fn station_packet_utf8_name_sig_verifies() {
        // 端到端：UTF-8 名称（"扬州FMO集群（苏）"）的 STATION 报文 → SIG 验签通过。
        // 线上名称与 TBS 第 9 元素同为 UTF-8 字节（官方文档 + 地图服务器实测依据），
        // 不再做 UTF-8→GBK 转换；这里走与 build_packet 相同的组包流程验证全链路。
        let kp = protocol::generate_keypair();
        let seed = kp["seed"].as_str().unwrap().to_string();
        let pubkey = kp["pubKey"].as_str().unwrap().to_string();
        let name = "扬州FMO集群（苏）";
        let lat_str = aprs_lat(32.39);
        let lon_str = aprs_lon(119.42);
        let cert_blob = b"fake-cert-blob".to_vec();
        let cert_hash = {
            use sha2::Digest;
            let mut h = sha2::Sha256::new();
            h.update(&cert_blob);
            h.finalize().to_vec()
        };
        let ts_slot = (now() / 600) as u64;
        let tbs = build_station_tbs(
            "BD4XGT", 15, &lat_str, &lon_str, &cert_hash, "CN", name,
            "fmo.example.com", 1883, 100, 3, 9, ts_slot,
        );
        let sig = protocol::sign(&seed, &tbs).expect("签名应成功");
        // 组包：名称段必须是原始 UTF-8 字节（非 GBK）
        let mut line = format!(
            "BD4XGT-15>APFMO4,TCPIP*:={lat_str}F{lon_str}iFMO-V4,STATION,CERT:{cert},CN,",
            cert = protocol::b64url_encode(&cert_blob),
        )
        .into_bytes();
        line.extend_from_slice(name.as_bytes());
        line.extend_from_slice(
            format!(",fmo.example.com,P1883,F100KM,U3/9,SIG:{}", protocol::b64url_encode(&sig))
                .as_bytes(),
        );
        assert!(line.windows(name.len()).any(|w| w == name.as_bytes()));
        // 线上名称字节应与 TBS 内名称一致（UTF-8 直通）
        let gbk_name = encoding_rs::GBK.encode(name).0;
        assert!(!line.windows(gbk_name.len()).any(|w| w == gbk_name.as_ref()));
        // 拆出 SIG 验签（用证书公钥，与实网验签同路径）
        let text = String::from_utf8(line).expect("UTF-8 直通后整帧应是合法 UTF-8");
        let sig_b64 = text.rsplit(",SIG:").next().unwrap();
        let sig_bytes = protocol::b64url_decode(sig_b64).expect("SIG 应可解码");
        assert!(protocol::verify(&pubkey, &tbs, &sig_bytes), "SIG 验签应通过");
    }

    #[test]
    fn legacy_config_without_ssid_defaults_to_zero() {
        // 8-12 时代的 broadcast.json 没有 ssid 字段，反序列化必须兼容
        let text = r#"{"mode_min":0,"name":"","host":"","port":1883,
            "cover_km":5000,"online":0,"peak":0,"country":"CN",
            "lat":39.9,"lon":116.4}"#;
        let cfg: BroadcastConfig = serde_json::from_str(text).unwrap();
        assert_eq!(cfg.ssid, 0);
    }

    #[test]
    fn effective_value_zero_means_auto() {
        // 0 = 自动（取花名册统计值），>0 = 手动覆盖
        assert_eq!(effective_value(0, 7), 7);
        assert_eq!(effective_value(0, 0), 0);
        assert_eq!(effective_value(5, 7), 5);
        assert_eq!(effective_value(5, 0), 5);
    }

    #[test]
    fn aprs_position_format() {
        // 与实捕位置前缀 =3952.80NF11931.57Ei 同格式
        assert_eq!(aprs_lat(39.88), "3952.80N");
        assert_eq!(aprs_lat(-33.865), "3351.90S");
        assert_eq!(aprs_lon(119.526), "11931.56E");
        assert_eq!(aprs_lon(-74.006), "07400.36W");
    }

    #[test]
    fn gate_requires_connected_super_and_own_server() {
        // 满足全部条件：已连接 + super + 登录自己服务器（大小写不敏感）
        assert!(gate_decide("connected", "super", "bd4xgt", "BD4XGT").is_ok());
        // MQTT 未连接
        let e = gate_decide("disconnected", "super", "BD4XGT", "BD4XGT").unwrap_err();
        assert!(e.contains("MQTT 未连接"), "应提示未连接: {e}");
        // 角色为 user / 空（连接中尚未确定角色）
        let e = gate_decide("connected", "user", "BD4XGT", "BD4XGT").unwrap_err();
        assert!(e.contains("super"), "应提示需 super: {e}");
        assert!(gate_decide("connected", "", "BD4XGT", "BD4XGT").is_err());
        // admin 暂不等同 super（待实网验证，先从严拒绝）
        let e = gate_decide("connected", "admin", "BD4XGT", "BD4XGT").unwrap_err();
        assert!(e.contains("super"), "admin 不应放行: {e}");
        // 登录的是别人的服务器
        let e = gate_decide("connected", "super", "BG8LLD", "BD4XGT").unwrap_err();
        assert!(e.contains("不是自己的服务器"), "应提示非自己服务器: {e}");
        // 服务器呼号缺失（未选定/无证书）
        assert!(gate_decide("connected", "super", "", "BD4XGT").is_err());
        // 服务器呼号带 SSID 时应剥离后放行（"BD4XGT-14" == "BD4XGT"）
        assert!(gate_decide("connected", "super", "BD4XGT-14", "bd4xgt").is_ok());
        assert!(gate_decide("connected", "super", "BD4XGT", "BD4XGT-7").is_ok());
    }

    #[test]
    fn station_tbs_is_16_element_cbor() {
        // 实网实锤布局（30/30 台 STATION 验签通过）：
        // ["FMO",4,"STATION",呼号大写,ssid,latStr,lonStr,sha256(certBlob),国家,名(UTF-8),host,port,km,在线,峰值,ts/600]
        let tbs = build_station_tbs(
            "BD4XGT",
            0,
            "3952.80N",
            "11931.57E",
            &[7u8; 32],
            "CN",
            "测试台",
            "fmo.example.com",
            1883,
            100,
            3,
            9,
            2987654,
        );
        let Some(protocol::CborValue::Array(arr)) = protocol::cbor_decode(&tbs) else {
            panic!("TBS 应为 CBOR 数组");
        };
        assert_eq!(arr.len(), 16);
        assert_eq!(arr[0], protocol::CborValue::Text("FMO".into()));
        assert_eq!(arr[1], protocol::CborValue::UInt(4));
        assert_eq!(arr[2], protocol::CborValue::Text("STATION".into()));
        assert_eq!(arr[3], protocol::CborValue::Text("BD4XGT".into()));
        assert_eq!(arr[4], protocol::CborValue::UInt(0));
        assert_eq!(arr[5], protocol::CborValue::Text("3952.80N".into()));
        assert_eq!(arr[7], protocol::CborValue::Bytes(vec![7u8; 32]));
        assert_eq!(arr[15], protocol::CborValue::UInt(2987654));
    }

    // ------------------------------------------------------------ BEACON

    fn decode_tbs(tbs: &[u8]) -> Vec<protocol::CborValue> {
        let Some(protocol::CborValue::Array(arr)) = protocol::cbor_decode(tbs) else {
            panic!("TBS 应为 CBOR 数组");
        };
        arr
    }

    #[test]
    fn beacon_tbs_omission_layouts() {
        // 实网实锤布局（官方文档 + 9/9 实捕验签通过）：
        // ["FMO",4,"BEACON",呼号大写,ssid,latStr,lonStr,sha256(certBlob),freqStr,
        //  (height>0),(rig非空),(ant非空),ts/600]，可选元素严格省略
        // 10 元素：无高度/电台/天线
        let arr = decode_tbs(&build_beacon_tbs(
            "ba4tcs", 15, "3202.39N", "12015.69E", &[9u8; 32], "431.0000", 0, "", "", 2987654,
        ));
        assert_eq!(arr.len(), 10);
        assert_eq!(arr[0], protocol::CborValue::Text("FMO".into()));
        assert_eq!(arr[1], protocol::CborValue::UInt(4));
        assert_eq!(arr[2], protocol::CborValue::Text("BEACON".into()));
        assert_eq!(arr[3], protocol::CborValue::Text("BA4TCS".into()));
        assert_eq!(arr[4], protocol::CborValue::UInt(15));
        assert_eq!(arr[5], protocol::CborValue::Text("3202.39N".into()));
        assert_eq!(arr[6], protocol::CborValue::Text("12015.69E".into()));
        assert_eq!(arr[7], protocol::CborValue::Bytes(vec![9u8; 32]));
        assert_eq!(arr[8], protocol::CborValue::Text("431.0000".into()));
        assert_eq!(arr[9], protocol::CborValue::UInt(2987654));
        // 11 元素：仅高度（rig/ant 省略时高度仍在 freq 之后）
        let arr = decode_tbs(&build_beacon_tbs(
            "BA4TCS", 15, "3202.39N", "12015.69E", &[9u8; 32], "431.0000", 18, "", "", 2987654,
        ));
        assert_eq!(arr.len(), 11);
        assert_eq!(arr[9], protocol::CborValue::UInt(18));
        assert_eq!(arr[10], protocol::CborValue::UInt(2987654));
        // 13 元素：高度 + 电台 + 天线（rig/ant 为 UTF-8 文本，线上同字节）
        let arr = decode_tbs(&build_beacon_tbs(
            "BA4TCS", 15, "3202.39N", "12015.69E", &[9u8; 32], "431.0000", 18,
            "海能达PDC580", "QTH江苏靖江", 2987654,
        ));
        assert_eq!(arr.len(), 13);
        assert_eq!(arr[9], protocol::CborValue::UInt(18));
        assert_eq!(arr[10], protocol::CborValue::Text("海能达PDC580".into()));
        assert_eq!(arr[11], protocol::CborValue::Text("QTH江苏靖江".into()));
        assert_eq!(arr[12], protocol::CborValue::UInt(2987654));
        // 12 元素变体：无高度但有 rig/ant 时同样省略 height 位
        let arr = decode_tbs(&build_beacon_tbs(
            "BA4TCS", 0, "3202.39N", "12015.69E", &[9u8; 32], "431.0000", 0, "R", "A", 2987654,
        ));
        assert_eq!(arr.len(), 12);
        assert_eq!(arr[9], protocol::CborValue::Text("R".into()));
    }

    #[test]
    fn beacon_frame_matches_captured_layout() {
        // 对齐实捕帧 BA4TCS-15 布局：RIG/ANT 线上 UTF-8、可选段省略、SIG 在末尾
        let cfg = BeaconConfig {
            ssid: 15,
            rig: "海能达PDC580".into(),
            freq_mhz: 431.0,
            ant: "QTH江苏靖江".into(),
            height_m: 18,
            ..Default::default()
        };
        let frame = build_beacon_frame(
            "BA4TCS-15", "3202.39N", "12015.69E", "CERT64", &cfg, "SIG64",
        )
        .expect("帧应构造成功");
        let head = b"BA4TCS-15>APFMO4,TCPIP*:=3202.39NF12015.69EiFMO-V4,BEACON,CERT:CERT64,FREQ:431.0000,HEIGHT:18,RIG:";
        assert!(frame.starts_with(head));
        let mut tail = "海能达PDC580".as_bytes().to_vec();
        tail.extend_from_slice(b",ANT:");
        tail.extend_from_slice("QTH江苏靖江".as_bytes());
        tail.extend_from_slice(b",SIG:SIG64");
        assert!(frame.ends_with(&tail));
        // 省略组合：height=0 且无 rig/ant 时 FREQ 之后直接 SIG
        let bare = BeaconConfig {
            freq_mhz: 145.5,
            ..Default::default()
        };
        let frame = build_beacon_frame("N0CALL", "3952.80N", "11931.57E", "C", &bare, "S").unwrap();
        assert_eq!(
            frame,
            b"N0CALL>APFMO4,TCPIP*:=3952.80NF11931.57EiFMO-V4,BEACON,CERT:C,FREQ:145.5000,SIG:S".to_vec()
        );
    }

    #[test]
    fn beacon_frame_rejects_over_512_bytes() {
        // 官方限制：整条帧 ≤512，超长拒绝发送（用超长证书 blob 撑爆）
        let cfg = BeaconConfig {
            freq_mhz: 431.0,
            ..Default::default()
        };
        let long_cert = "A".repeat(600);
        let err = build_beacon_frame("N0CALL", "3952.80N", "11931.57E", &long_cert, &cfg, "S")
            .unwrap_err();
        assert!(err.contains("512"), "应提示 512 上限: {err}");
    }

    #[test]
    fn beacon_config_validation() {
        let ok = BeaconConfig {
            ssid: 15,
            rig: "海能达PDC580".into(),
            freq_mhz: 431.0,
            ant: "QTH江苏靖江".into(),
            height_m: 18,
            aprs_msg: "73 de BD4XGT".into(),
            notice: "欢迎通联".into(),
            qso_msg: "祝通联愉快，73！".into(),
            enabled: true,
        };
        assert!(ok.validate().is_ok());
        // freq=0（未配置）合法，发送时才门控
        assert!(BeaconConfig::default().validate().is_ok());
        // 字符数超限
        assert!(BeaconConfig { rig: "台".repeat(17), ..Default::default() }.validate().is_err());
        assert!(BeaconConfig { ant: "a".repeat(17), ..Default::default() }.validate().is_err());
        assert!(BeaconConfig { aprs_msg: "m".repeat(65), ..Default::default() }.validate().is_err());
        assert!(BeaconConfig { notice: "n".repeat(129), ..Default::default() }.validate().is_err());
        assert!(BeaconConfig { qso_msg: "q".repeat(129), ..Default::default() }.validate().is_err());
        // 英文逗号（会破坏报文逗号分隔）
        assert!(BeaconConfig { rig: "a,b".into(), ..Default::default() }.validate().is_err());
        assert!(BeaconConfig { notice: "a,b".into(), ..Default::default() }.validate().is_err());
        // 线上 UTF-8：emoji 等任意 Unicode 文本放行（官方文档允许 UTF-8 文本）
        assert!(BeaconConfig { aprs_msg: "hi🎙".into(), ..Default::default() }.validate().is_ok());
        // 频率范围 20-500（含 NaN/负数/超范围）
        for bad in [19.9, 500.1, -1.0, f64::NAN] {
            assert!(
                BeaconConfig { freq_mhz: bad, ..Default::default() }.validate().is_err(),
                "freq={bad} 应拒绝"
            );
        }
        assert!(BeaconConfig { freq_mhz: 20.0, ..Default::default() }.validate().is_ok());
        assert!(BeaconConfig { freq_mhz: 500.0, ..Default::default() }.validate().is_ok());
    }

    #[test]
    fn legacy_beacon_json_missing_fields_default() {
        // serde(default) 全字段兜底：空对象/缺字段的旧 beacon.json 都能反序列化
        let cfg: BeaconConfig = serde_json::from_str("{}").unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.ssid, 0);
        assert_eq!(cfg.freq_mhz, 0.0);
        assert_eq!(cfg.height_m, 0);
        assert!(cfg.rig.is_empty() && cfg.ant.is_empty());
        assert!(cfg.aprs_msg.is_empty() && cfg.notice.is_empty() && cfg.qso_msg.is_empty());
        let cfg: BeaconConfig =
            serde_json::from_str(r#"{"enabled":true,"ssid":15,"freq_mhz":431.0}"#).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.ssid, 15);
        assert_eq!(cfg.freq_mhz, 431.0);
        assert!(cfg.notice.is_empty());
    }

    #[test]
    fn apfmo1_apfmo2_frames() {
        // APFMO1 登录公告：名称 UTF-8 + 固定状态段 + 公告（空则省略最后一段）
        let f = build_apfmo1_frame("BG9JYT-14", "无锡FMO", 12, 100, "欢迎通联");
        let mut expect = b"BG9JYT-14>APFMO1,TCPIP*:>".to_vec();
        expect.extend_from_slice("无锡FMO".as_bytes());
        expect.extend_from_slice(",正常,在线/峰值:12/100,".as_bytes());
        expect.extend_from_slice("欢迎通联".as_bytes());
        assert_eq!(f, expect);
        // 公告为空：省略最后一段（连逗号一起）
        let f = build_apfmo1_frame("BG9JYT", "无锡FMO", 0, 7, "");
        assert!(f.ends_with(",正常,在线/峰值:0/7".as_bytes()));
        // APFMO2 个性化消息：无签名，正文 UTF-8
        let f = build_apfmo2_frame("BA4TCS-15", "73 de 江苏");
        let mut expect = b"BA4TCS-15>APFMO2,TCPIP*:>".to_vec();
        expect.extend_from_slice("73 de 江苏".as_bytes());
        assert_eq!(f, expect);
    }

    /// 回归：网格配置——留空由经纬度推导；手填合法值优先（大写）；
    /// 非法值回退推导。
    #[test]
    fn effective_grid_manual_overrides_and_fallback() {
        let mut cfg = BroadcastConfig::default(); // 默认北京 39.9/116.4
        assert_eq!(cfg.effective_grid(), "OM89ev");
        cfg.grid = "om92jd".into();
        assert_eq!(cfg.effective_grid(), "OM92JD");
        cfg.grid = "XX!!".into(); // 非法 → 回退推导
        assert_eq!(cfg.effective_grid(), "OM89ev");
    }
}
