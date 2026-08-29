//! FMO device activation: POST /api/device/activate to the certificate server
//! with an Ed25519-signed deterministic CBOR request. On success the returned
//! userCert/intermediateCert are written into the cert store and FMO (MQTT)
//! reconnects with the new identity.
//!
//! Ported from firmware src/services/fmo_activate.cpp / fmo_activate_core.cpp;
//! protocol details in docs/fmo-device-activate-api.md.

use crate::fmo::certstore::CertStore;
use crate::fmo::protocol::{self, CborValue};
use crate::fmo::state::FmoState;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const DEFAULT_SERVER: &str = "www.hamptt.com";
const ACTIVATE_PATH: &str = "/api/device/activate";
const COUNTRY_CODE: &str = "CN";
const CONFIG_FILE: &str = "activate_config.json";

fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join(CONFIG_FILE)
}

/// Certificate server host (default www.hamptt.com), persisted in
/// fmo/activate_config.json.
pub fn get_server(data_dir: &Path) -> String {
    if let Ok(text) = std::fs::read_to_string(config_path(data_dir)) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            if let Some(s) = v.get("server").and_then(Value::as_str) {
                let s = s.trim();
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
    }
    DEFAULT_SERVER.to_string()
}

pub fn set_server(data_dir: &Path, server: &str) -> Result<(), String> {
    let cleaned = server.trim().trim_end_matches('/');
    if cleaned.is_empty() || cleaned.len() > 128 {
        return Err("证书服务器地址为空或过长".into());
    }
    let text = serde_json::to_string_pretty(&json!({ "server": cleaned }))
        .map_err(|e| e.to_string())?;
    std::fs::write(config_path(data_dir), text).map_err(|e| format!("保存激活配置失败: {e}"))
}

/// Snapshot returned to the UI: configured server + this machine's MAC (the
/// address that must be registered/bound on the platform).
pub fn config(data_dir: &Path) -> Value {
    json!({
        "server": get_server(data_dir),
        "mac": local_mac_string(),
    })
}

/// MAC of the default network interface; this is the address the user
/// registers/binds on the platform.
pub fn local_mac() -> Result<[u8; 6], String> {
    let mac = mac_address::get_mac_address()
        .map_err(|e| format!("读取本机 MAC 地址失败: {e}"))?
        .ok_or_else(|| "未找到可用网卡的 MAC 地址，无法激活".to_string())?;
    Ok(mac.bytes())
}

/// Uppercase 12-hex-digit MAC for display; empty string when unavailable.
pub fn local_mac_string() -> String {
    local_mac()
        .map(|m| hex::encode(m).to_uppercase())
        .unwrap_or_default()
}

/// Load the device Ed25519 key from the cert store, generating and persisting a
/// fresh keypair on first activation. The seed (private key) never leaves this
/// machine. Returns (seed_b64, public_key).
pub async fn ensure_device_key(store: &CertStore) -> Result<(String, [u8; 32]), String> {
    let path = store.dir.join("cert_devicekey.json");
    if path.is_file() {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cert_devicekey.json: {e}"))?;
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| format!("cert_devicekey.json: {e}"))?;
        let seed = v
            .get("seed")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let pub_key = v
            .get("pubKey")
            .and_then(Value::as_str)
            .and_then(protocol::decode_seed)
            .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok());
        match (seed.is_empty(), pub_key) {
            (false, Some(pk)) => return Ok((seed, pk)),
            _ => return Err("设备密钥文件损坏，请手动导入 deviceKey JSON".into()),
        }
    }

    let mut obj = protocol::generate_keypair();
    obj.insert("type".into(), Value::String("deviceKey".into()));
    let seed = obj["seed"].as_str().unwrap_or("").to_string();
    let pub_key: [u8; 32] = protocol::decode_seed(obj["pubKey"].as_str().unwrap_or(""))
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| "设备密钥生成失败".to_string())?;
    store
        .import_json("cert_devicekey", Value::Object(obj), "activate")
        .await;
    Ok((seed, pub_key))
}

/// SHA-256 of the running executable; falls back to hashing the version string
/// when the exe cannot be read.
fn firmware_hash() -> [u8; 32] {
    if let Ok(exe) = std::env::current_exe() {
        if let Ok(bytes) = std::fs::read(&exe) {
            return Sha256::digest(&bytes).into();
        }
    }
    Sha256::digest(env!("CARGO_PKG_VERSION").as_bytes()).into()
}

/// All fields that go into the signed activate request.
pub struct ActivateFields<'a> {
    pub mac: [u8; 6],
    pub timestamp: u64,
    pub nonce: [u8; 6],
    pub firmware_version: &'a str,
    pub firmware_hash: [u8; 32],
    pub country_code: &'a str,
    pub device_public_key: [u8; 32],
}

/// Deterministic CBOR of the 10-element activateReq array (API doc §3).
/// The MAC is a 6-byte byte string, not text.
pub fn build_request_cbor(f: &ActivateFields) -> Vec<u8> {
    protocol::cbor_tbs(&[
        CborValue::Text("FMO".into()),
        CborValue::UInt(4),
        CborValue::Text("activateReq".into()),
        CborValue::Bytes(f.mac.to_vec()),
        CborValue::UInt(f.timestamp),
        CborValue::Bytes(f.nonce.to_vec()),
        CborValue::Text(f.firmware_version.to_string()),
        CborValue::Bytes(f.firmware_hash.to_vec()),
        CborValue::Text(f.country_code.to_string()),
        CborValue::Bytes(f.device_public_key.to_vec()),
    ])
}

/// JSON request body; all byte fields are hex (MAC uppercase, rest lowercase).
pub fn build_request_json(f: &ActivateFields, signature: &[u8]) -> Value {
    json!({
        "version": "4",
        "action": "activate",
        "mac": hex::encode(f.mac).to_uppercase(),
        "timestamp": f.timestamp,
        "firmwareVersion": f.firmware_version,
        "firmwareHash": hex::encode(f.firmware_hash),
        "countryCode": f.country_code,
        "devicePublicKey": hex::encode(f.device_public_key),
        "nonce": hex::encode(f.nonce),
        "signature": hex::encode(signature),
    })
}

/// Chinese messages for the platform error codes (firmware codeMessage()).
fn code_message(code: i64) -> Option<&'static str> {
    match code {
        1 => Some("本机 MAC 未绑定用户：请先在 hamptt.com 登记并绑定本机 MAC"),
        2 => Some("本机 MAC 未登记：请先在 hamptt.com 登记并绑定本机 MAC"),
        3 => Some("时间戳误差过大：请检查系统时间同步"),
        4 => Some("请求被判为重放：请稍后重试"),
        5 => Some("请求格式错误"),
        6 => Some("国家码受限"),
        7 => Some("设备或用户已被封禁"),
        8 => Some("超出申请频率限制（每 MAC 每小时 5 次）：请稍后重试"),
        9 => Some("设备签名验证失败"),
        10 => Some("平台 CA 未配置"),
        100 => Some("已转人工审核：请审核通过后重试"),
        _ => None,
    }
}

/// Run one activation round. Returns a human-readable success message; errors
/// are already mapped to Chinese hints.
pub async fn run(fmo: &FmoState) -> Result<String, String> {
    let mac = local_mac()?;
    let (seed, pub_key) = ensure_device_key(&fmo.cert_store).await?;

    let fields = ActivateFields {
        mac,
        timestamp: protocol::now_ts() as u64,
        nonce: rand::random(),
        firmware_version: env!("CARGO_PKG_VERSION"),
        firmware_hash: firmware_hash(),
        country_code: COUNTRY_CODE,
        device_public_key: pub_key,
    };
    let cbor = build_request_cbor(&fields);
    let signature = protocol::sign(&seed, &cbor)?;
    let body = build_request_json(&fields, &signature);

    let server = get_server(&fmo.data_dir);
    let base = if server.starts_with("http://") || server.starts_with("https://") {
        server.trim_end_matches('/').to_string()
    } else {
        format!("https://{}", server.trim_end_matches('/'))
    };
    let client = crate::platform::http_client()?;
    let response: Value =
        crate::platform::post_json_exact(&client, &base, ACTIVATE_PATH, None, &body).await?;

    let result = response.get("result").and_then(Value::as_str).unwrap_or("");
    let code = response.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if result != "ok" {
        if let Some(msg) = code_message(code) {
            return Err(msg.to_string());
        }
        let reason = response.get("reason").and_then(Value::as_str).unwrap_or("");
        return Err(if reason.is_empty() {
            format!("激活失败 code={code}")
        } else {
            format!("激活失败 code={code}：{reason}")
        });
    }

    let package = response.get("certPackage").cloned().unwrap_or(Value::Null);
    let user_cert = package.get("userCert").cloned().unwrap_or(Value::Null);
    let int_cert = package
        .get("intermediateCert")
        .cloned()
        .unwrap_or(Value::Null);
    if !user_cert.is_object() || !int_cert.is_object() {
        return Err("激活响应缺少证书包（certPackage）".into());
    }
    fmo.cert_store
        .import_json("cert_user", user_cert, "activate")
        .await;
    fmo.cert_store
        .import_json("cert_int", int_cert, "activate")
        .await;

    // Identity self-check before touching MQTT (key must match the new cert).
    let certs_dir = fmo.data_dir.join("certs");
    crate::fmo::fmo_auth::validate_identity(&certs_dir)?;

    let (callsign, uid) = crate::fmo::state::read_identity(&fmo.data_dir, "");
    let mut msg = format!("OK 已获取证书：{callsign} / UID {uid}");
    (fmo.emit)(json!({"type": "log", "level": "info", "msg": msg.clone()}));

    // Reconnect FMO (MQTT) with the new identity when it was connected.
    if fmo.mqtt_client.state_str().await == "connected" {
        fmo.disconnect_mqtt().await;
        if let Err(e) = fmo.connect_mqtt(false).await {
            (fmo.emit)(json!({"type": "log", "level": "warn",
                "msg": format!("激活后 MQTT 重连失败：{e}")}));
            msg = format!("{msg}（MQTT 重连失败：{e}）");
        }
    }
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden vector from docs/fmo-device-activate-api.md §3 (same as firmware
    /// tests/fmo_activate_test.cpp): byte-exact CBOR comparison.
    #[test]
    fn activate_cbor_golden_vector() {
        let fields = ActivateFields {
            mac: [0xD0, 0xCF, 0x13, 0x51, 0x0C, 0x4C],
            timestamp: 1783291391,
            nonce: [0x01, 0x02, 0x03, 0x04, 0x05, 0x06],
            firmware_version: "4.0.0",
            firmware_hash: [0x07; 32],
            country_code: "CN",
            device_public_key: hex::decode(
                "ED4928C628D1C2C6EAE90338905995612959273A5C63F93636C14614AC8737D1",
            )
            .unwrap()
            .try_into()
            .unwrap(),
        };
        let encoded = build_request_cbor(&fields);
        let expect = "8A63464D4F046B616374697661746552657146D0CF13510C4C1A6A4ADDFF\
4601020304050665342E302E30582007070707070707070707070707070707070707\
0707070707070707070707070762434E5820ED4928C628D1C2C6EAE903389059\
95612959273A5C63F93636C14614AC8737D1";
        assert_eq!(hex::encode(&encoded).to_uppercase(), expect);
    }

    #[test]
    fn request_json_field_shapes() {
        let fields = ActivateFields {
            mac: [0xD0, 0xCF, 0x13, 0x51, 0x0C, 0x4C],
            timestamp: 1783291391,
            nonce: [0x01, 0x02, 0x03, 0x04, 0x05, 0x06],
            firmware_version: "4.0.0",
            firmware_hash: [0x07; 32],
            country_code: "CN",
            device_public_key: [0xED; 32],
        };
        let body = build_request_json(&fields, &[0xAB; 64]);
        assert_eq!(body["version"], "4");
        assert_eq!(body["action"], "activate");
        assert_eq!(body["mac"], "D0CF13510C4C");
        assert_eq!(body["timestamp"], 1783291391);
        assert_eq!(body["firmwareVersion"], "4.0.0");
        assert_eq!(body["firmwareHash"].as_str().unwrap().len(), 64);
        assert_eq!(body["countryCode"], "CN");
        assert_eq!(body["devicePublicKey"].as_str().unwrap().len(), 64);
        assert_eq!(body["nonce"], "010203040506");
        assert_eq!(body["signature"].as_str().unwrap().len(), 128);
    }
}
