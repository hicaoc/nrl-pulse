//! FMO 服务器广播（FMO-V4 STATION，APRS APFMO4 位置包）。
//!
//! 协议逆向自原厂固件（fmo-sim/docs/firmware-analysis.md §8.3）：
//! - 报文：`CS>APFMO4,TCPIP*:=<位置>F<经度>iFMO-V4,STATION,CERT:<b64url CBOR>,<国家>,<GBK名称>,<host>,P<port>,F<覆盖>KM,U<在线>/<峰值>,SIG:<b64url 64B Ed25519>`
//! - 周期：5/10/60 分钟可配（原厂默认 10 分钟）；手动广播有 60s 最小间隔（同原厂限速）
//! - CERT = 本机用户证书的 10 元素 CBOR；SIG = 设备私钥（cert_devicekey）对 TBS 的 Ed25519 签名
//!   （TBS 16 元素布局已实网验签实锤，见 build_station_tbs 注释）

use crate::fmo::aprs::{AprsTx, EmitFn};
use crate::fmo::protocol;
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

/// 手动/自动广播的最小间隔（原厂 sendV4Station 的 60s 速率限制）
const MIN_INTERVAL_S: i64 = 60;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct BroadcastConfig {
    /// 自动广播周期（分钟）：0=关闭，5/10/60
    pub mode_min: u32,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub cover_km: u32,
    pub online: u32,
    pub peak: u32,
    pub country: String,
    pub lat: f64,
    pub lon: f64,
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
        }
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
///   7 SHA-256(完整 CERT CBOR blob) 32B | 8 国家码大写 | 9 名称（UTF-8！线上是 GBK）
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

impl BroadcastEngine {
    pub fn new(emit: EmitFn, tx: Arc<AprsTx>, data_dir: PathBuf) -> Self {
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
        }
    }

    pub async fn config(&self) -> BroadcastConfig {
        self.cfg.lock().await.clone()
    }

    pub async fn set_config(&self, cfg: BroadcastConfig) {
        {
            let mut guard = self.cfg.lock().await;
            *guard = cfg;
        }
        let guard = self.cfg.lock().await;
        if let Ok(text) = serde_json::to_string_pretty(&*guard) {
            std::fs::write(&self.cfg_path, text).ok();
        }
    }

    /// 本机用户证书 → (呼号, CERT blob 原始字节, b64url(blob), 设备私钥 seed)
    fn cert_blob(&self) -> Result<(String, Vec<u8>, String, String), String> {
        let certs = self.data_dir.join("certs");
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

    /// 构造完整 STATION 广播报文（不含换行）
    fn build_packet(&self, cfg: &BroadcastConfig) -> Result<Vec<u8>, String> {
        let (callsign, cert_blob, cert_b64, seed) = self.cert_blob()?;
        let name_gbk: Vec<u8> = {
            let (cow, _, _) = encoding_rs::GBK.encode(&cfg.name);
            cow.into_owned()
        };
        let lat_str = aprs_lat(cfg.lat);
        let lon_str = aprs_lon(cfg.lon);
        // SIG：TBS 第 7 元素 = SHA-256(完整 CERT blob)，名称用 UTF-8（线上才是 GBK）
        let cert_hash = {
            use sha2::Digest;
            let mut h = sha2::Sha256::new();
            h.update(&cert_blob);
            h.finalize().to_vec()
        };
        let ts_slot = (now() / 600) as u64;
        // 呼号无 SSID（证书呼号），ssid=0
        let tbs = build_station_tbs(
            &callsign,
            0,
            &lat_str,
            &lon_str,
            &cert_hash,
            &cfg.country,
            &cfg.name,
            &cfg.host,
            cfg.port,
            cfg.cover_km,
            cfg.online,
            cfg.peak,
            ts_slot,
        );
        let sig = protocol::sign(&seed, &tbs)?;
        let mut line = format!(
            "{cs}>APFMO4,TCPIP*:={lat}F{lon}iFMO-V4,STATION,CERT:{cert},{country},",
            cs = callsign,
            lat = lat_str,
            lon = lon_str,
            cert = cert_b64,
            country = cfg.country,
        )
        .into_bytes();
        line.extend_from_slice(&name_gbk);
        line.extend_from_slice(
            format!(
                ",{host},P{port},F{cov}KM,U{on}/{peak},SIG:{sig}",
                host = cfg.host,
                port = cfg.port,
                cov = cfg.cover_km,
                on = cfg.online,
                peak = cfg.peak,
                sig = protocol::b64url_encode(&sig),
            )
            .as_bytes(),
        );
        Ok(line)
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
        if *self.tx.state.lock().await != "verified" {
            return Err("APRS 上行未验证登录（先连接 APRS 且 passcode 正确）".into());
        }
        let line = self.build_packet(&cfg)?;
        let preview = String::from_utf8_lossy(&line).into_owned();
        self.tx.send_packet(line).await?;
        *self.last_sent.lock().await = t;
        (self.emit)(json!({"type": "log", "level": "info",
            "msg": format!("服务器广播已发送（{}:{} 在线{}/{}）：{}",
                           cfg.host, cfg.port, cfg.online, cfg.peak, preview)}));
        (self.emit)(json!({"type": "broadcast_state", "lastSent": t}));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aprs_position_format() {
        // 与实捕位置前缀 =3952.80NF11931.57Ei 同格式
        assert_eq!(aprs_lat(39.88), "3952.80N");
        assert_eq!(aprs_lat(-33.865), "3351.90S");
        assert_eq!(aprs_lon(119.526), "11931.56E");
        assert_eq!(aprs_lon(-74.006), "07400.36W");
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
}
