//! FMO 4.0 MQTT 认证载荷构造（SAS HTTP 认证）。
//!
//! - MQTT CONNECT：username = 呼号；password = base64url(JSON)
//! - JSON = {certPackage:{intermediateCert,userCert}, targetCallsign, targetUID,
//!   role, targetUrl, targetPort, serverFingerprint, timestamp, proof:{signature}}

use crate::fmo::protocol;
use sha2::Digest;
use std::path::Path;

pub fn user_cert_tbs(user_cert: &serde_json::Value) -> Vec<protocol::CborValue> {
    protocol::user_cert_tbs(user_cert)
}

pub fn cert_fingerprint(user_cert: &serde_json::Value) -> Vec<u8> {
    protocol::cert_fingerprint(user_cert)
}

/// 从 APRS 广播解析出的 CERT dict 算指纹。
pub fn beacon_cert_fingerprint(beacon_cert: &serde_json::Value) -> Vec<u8> {
    let pubkey_hex = beacon_cert["pubkey_hex"].as_str().unwrap_or("");
    let pubkey = hex::decode(pubkey_hex).unwrap_or_default();
    let tbs: Vec<protocol::CborValue> = vec![
        protocol::CborValue::Text("FMO".into()),
        protocol::CborValue::UInt(4),
        protocol::CborValue::Text("userCert".into()),
        protocol::CborValue::UInt(beacon_cert["alg"].as_u64().unwrap_or(0)),
        protocol::CborValue::Text(beacon_cert["callsign"].as_str().unwrap_or("").to_string()),
        protocol::CborValue::UInt(beacon_cert["uid"].as_u64().unwrap_or(0)),
        protocol::CborValue::Bytes(pubkey),
        protocol::CborValue::UInt(beacon_cert["iat"].as_u64().unwrap_or(0)),
        protocol::CborValue::UInt(beacon_cert["exp"].as_u64().unwrap_or(0)),
    ];
    let mut hasher = sha2::Sha256::new();
    hasher.update(protocol::cbor_tbs(&tbs));
    hasher.finalize().to_vec()
}

/// server = {"callsign", "uid", "host", "port", "fingerprint"(bytes)}
pub fn build_mqtt_password(user_cert: &serde_json::Value,
                           intermediate_cert: &serde_json::Value,
                           seed_b64: &str,
                           server: &serde_json::Value,
                           role: &str,
                           timestamp: Option<i64>) -> Result<String, String> {
    let ts = timestamp.unwrap_or_else(protocol::now_ts);
    let srv_fp = server["fingerprint"].as_array()
        .ok_or("fingerprint 缺失")?
        .iter().filter_map(|b| b.as_u64()).map(|b| b as u8).collect::<Vec<u8>>();
    let u_fp = cert_fingerprint(user_cert);

    let server_uid = server["uid"].as_u64().unwrap_or(0);
    let srv_callsign = server["callsign"].as_str().unwrap_or("").to_uppercase();
    let srv_host = server["host"].as_str().unwrap_or("");
    let srv_port = server["port"].as_u64().unwrap_or(0);

    let proof_tbs: Vec<protocol::CborValue> = vec![
        protocol::CborValue::Text("FMO".into()),
        protocol::CborValue::UInt(4),
        protocol::CborValue::Text("serverAuthorizerReqHttp".into()),
        protocol::CborValue::UInt(server_uid),
        protocol::CborValue::Text(srv_callsign),
        protocol::CborValue::UInt(server_uid),
        protocol::CborValue::Text(role.to_string()),
        protocol::CborValue::Text(srv_host.to_string()),
        protocol::CborValue::UInt(srv_port),
        protocol::CborValue::Bytes(srv_fp.clone()),
        protocol::CborValue::UInt(ts as u64),
        protocol::CborValue::Bytes(u_fp),
    ];
    let sig = protocol::sign(seed_b64, &protocol::cbor_tbs(&proof_tbs))?;

    let payload = serde_json::json!({
        "certPackage": {"intermediateCert": intermediate_cert, "userCert": user_cert},
        "targetCallsign": server["callsign"],
        "targetUID": server_uid,
        "role": role,
        "targetUrl": srv_host,
        "targetPort": srv_port,
        "serverFingerprint": protocol::b64url_encode(&srv_fp),
        "timestamp": ts,
        "proof": {"signature": protocol::b64url_encode(&sig)},
    });
    Ok(protocol::b64url_encode(&serde_json::to_vec(&payload).map_err(|e| e.to_string())?))
}

const BUILTIN_CERT_ROOT: &str = include_str!("../../builtin_certs/cert_root.json");
const BUILTIN_CERT_INT: &str = include_str!("../../builtin_certs/cert_int.json");

fn _load_cert(d: &Path, name: &str) -> Result<serde_json::Value, String> {
    let p = d.join(name);
    let text = if p.is_file() {
        std::fs::read_to_string(&p).map_err(|e| format!("{name}: {e}"))?
    } else if name == "cert_int.json" {
        BUILTIN_CERT_INT.to_string()
    } else if name == "cert_root.json" {
        BUILTIN_CERT_ROOT.to_string()
    } else {
        return Err(format!("{name}: 文件不存在 ({})", p.display()));
    };
    serde_json::from_str(&text).map_err(|e| format!("{name}: {e}"))
}

/// 从 certs_dir 加载 userCert/devicekey/intermediateCert。
pub fn load_identity(certs_dir: &Path) -> Result<serde_json::Value, String> {
    let user_cert = _load_cert(certs_dir, "cert_user.json")?;
    let intermediate_cert = _load_cert(certs_dir, "cert_int.json")?;
    let dk_text = std::fs::read_to_string(certs_dir.join("cert_devicekey.json"))
        .map_err(|e| format!("cert_devicekey.json: {e}"))?;
    let dk: serde_json::Value = serde_json::from_str(&dk_text)
        .map_err(|e| format!("cert_devicekey.json: {e}"))?;
    Ok(serde_json::json!({
        "user_cert": user_cert,
        "intermediate_cert": intermediate_cert,
        "seed": dk["seed"].clone(),
    }))
}

/// 一站式：返回 {username, password}。
pub fn mqtt_credentials(certs_dir: &Path, server: &serde_json::Value,
                        role: &str) -> Result<serde_json::Value, String> {
    let ident = load_identity(certs_dir)?;
    let pw = build_mqtt_password(
        &ident["user_cert"], &ident["intermediate_cert"],
        ident["seed"].as_str().unwrap_or(""),
        server, role, None)?;
    Ok(serde_json::json!({
        "username": ident["user_cert"]["subject"]["callsign"],
        "password": pw,
    }))
}

/// APRS passcode（标准算法，与 aprslib 一致）。
pub fn aprs_passcode(callsign: &str) -> String {
    let base = callsign.split('-').next().unwrap_or(callsign);
    let mut code: u32 = 0x73E2;
    for (i, c) in base.to_uppercase().bytes().enumerate() {
        let shift = if i % 2 == 0 { 8 } else { 0 };
        code ^= (c as u32) << shift;
    }
    (code & 0x7fff).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_fp_len() {
        let user = serde_json::json!({
            "issuerSn": 1, "iat": 1000, "exp": 31537000,
            "subject": {"callsign": "BG9JYT", "uid": 447, "publicKey": "MDEwDQYJKoZIhvcNAQEFBQADAQ"},
        });
        let fp = cert_fingerprint(&user);
        assert_eq!(fp.len(), 32);
    }

    #[test]
    fn aprs_passcode_known() {
        assert_eq!(aprs_passcode("W1AW"), "25988");
        assert_eq!(aprs_passcode("N0CALL"), "13023");
        assert_eq!(aprs_passcode("BG9JYT"), "20923");
        assert_eq!(aprs_passcode("BD4XGT"), "17066");
        assert_eq!(aprs_passcode("BD4XGT-7"), aprs_passcode("BD4XGT"));
    }

    #[test]
    fn build_credentials_roundtrip() {
        // 用用户真实证书 + 服务器 beacon cert 构建 SAS 凭据，确认流程与 sim-rust 一致
        let user_cert = serde_json::json!({
            "issuerSn": 1001, "iat": 1784899214, "exp": 1816435214,
            "subject": {"callsign": "BD4XGT", "uid": 796,
                        "publicKey": "M9pzGc4pnEiUzapOCdDGsUQhuAmH9lCDgfgRpRQgsoo"},
        });
        let int_cert = serde_json::json!({
            "sn": 1001, "type": "intermediateCA",
            "subject": {"name": "BG5ESN", "publicKey": "gYPN5agzrKZG2iyEztsVjGD1tVNLozHNm_km7n6OQyk"},
            "issuer": {"sn": 1, "name": "BG5ESN", "publicKey": "DCeeVS320f36ToVP2eOADVN-Q0LzpMYmiVkmNYzuysY"},
        });
        // 服务器 beacon cert（来自 STATION 广播）
        let server = serde_json::json!({
            "host": "fmo.panyong.top",
            "port": 64001,
            "callsign": "BI1THT",
            "uid": 1286,
            "cert": {
                "callsign": "BI1THT", "uid": 1286,
                "pubkey_hex": "4091883448b318fafcb8c523d1f1d89ceb54de5b23cf8447088ca381b98a7943",
                "alg": 1001, "iat": 1784975543, "exp": 1816511543,
            },
            "fingerprint": beacon_cert_fingerprint(&serde_json::json!({
                "callsign": "BI1THT", "uid": 1286,
                "pubkey_hex": "4091883448b318fafcb8c523d1f1d89ceb54de5b23cf8447088ca381b98a7943",
                "alg": 1001, "iat": 1784975543, "exp": 1816511543,
            })).into_iter().map(|b| serde_json::Value::from(b)).collect::<Vec<_>>(),
        });
        let pw = build_mqtt_password(&user_cert, &int_cert, "taIOylDLfECyOfG7PQFVO54jAGUbtMx7ztyYSQLUHGQ", &server, "user", Some(1786323504)).unwrap();
        // password 应为 base64url JSON
        // password 应为 base64url JSON
        let decoded = protocol::b64url_decode(&pw).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(v["targetCallsign"], "BI1THT");
        assert_eq!(v["targetUID"], 1286);
        assert_eq!(v["targetUrl"], "fmo.panyong.top");
        assert_eq!(v["targetPort"], 64001);
        assert_eq!(v["role"], "user");
        assert!(!v["proof"]["signature"].as_str().unwrap().is_empty());
        // 再验证 fingerprint 长度 32
        let fp = beacon_cert_fingerprint(&server["cert"]);
        assert_eq!(fp.len(), 32);
    }
}
