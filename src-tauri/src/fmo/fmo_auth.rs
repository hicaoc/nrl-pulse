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
pub fn build_mqtt_password(
    user_cert: &serde_json::Value,
    intermediate_cert: &serde_json::Value,
    seed_b64: &str,
    server: &serde_json::Value,
    role: &str,
    timestamp: Option<i64>,
) -> Result<String, String> {
    let ts = timestamp.unwrap_or_else(protocol::now_ts);
    let srv_fp = server["fingerprint"]
        .as_array()
        .ok_or("fingerprint 缺失")?
        .iter()
        .filter_map(|b| b.as_u64())
        .map(|b| b as u8)
        .collect::<Vec<u8>>();
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
    Ok(protocol::b64url_encode(
        &serde_json::to_vec(&payload).map_err(|e| e.to_string())?,
    ))
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
    let dk: serde_json::Value =
        serde_json::from_str(&dk_text).map_err(|e| format!("cert_devicekey.json: {e}"))?;
    Ok(serde_json::json!({
        "user_cert": user_cert,
        "intermediate_cert": intermediate_cert,
        "seed": dk["seed"].clone(),
    }))
}

/// 一站式：返回 {username, password, role}。
pub fn mqtt_credentials(
    certs_dir: &Path,
    server: &serde_json::Value,
    role: &str,
) -> Result<serde_json::Value, String> {
    let ident = load_identity(certs_dir)?;
    let pw = build_mqtt_password(
        &ident["user_cert"],
        &ident["intermediate_cert"],
        ident["seed"].as_str().unwrap_or(""),
        server,
        role,
        None,
    )?;
    Ok(serde_json::json!({
        "username": ident["user_cert"]["subject"]["callsign"],
        "password": pw,
        "role": role,
    }))
}

/// 初始角色选择：登录服务器呼号与证书呼号一致（自己的服务器）默认 super，
/// 否则默认 user；被拒后 MQTT 客户端按 ROLE_SEQ 从该角色起继续往后重试。
pub fn initial_role(certs_dir: &Path, server: &serde_json::Value) -> String {
    let cert_cs = load_identity(certs_dir)
        .ok()
        .and_then(|i| {
            i["user_cert"]["subject"]["callsign"]
                .as_str()
                .map(|s| s.to_uppercase())
        })
        .unwrap_or_default();
    let srv_cs = server["callsign"].as_str().unwrap_or("").to_uppercase();
    if !cert_cs.is_empty() && !srv_cs.is_empty() && cert_cs == srv_cs {
        "super".to_string()
    } else {
        "user".to_string()
    }
}

/// 连接前身份自检：证书过期、devicekey 私钥与 user 证书公钥不配套时，
/// 签名在所有服务器都验不过（NotAuthorized），提前给出可定位的中文原因。
pub fn validate_identity(certs_dir: &Path) -> Result<(), String> {
    let ident = load_identity(certs_dir)?;
    let user_cert = &ident["user_cert"];
    // 1. 证书有效期
    let now = protocol::now_ts();
    if let Some(exp) = user_cert["exp"].as_u64() {
        if (exp as i64) <= now {
            return Err(format!("用户证书已过期（exp={exp}），请重新申请并导入证书"));
        }
    }
    // 2. devicekey 私钥推导的公钥必须与 user 证书里的公钥一致（不是一套则所有服务器都拒）
    let seed = ident["seed"].as_str().unwrap_or("");
    let cert_pk = user_cert["subject"]["publicKey"].as_str().unwrap_or("");
    if !seed.is_empty() && !cert_pk.is_empty() {
        if let (Some(derived), Some(in_cert)) = (
            protocol::pubkey_from_seed(seed),
            protocol::decode_seed(cert_pk),
        ) {
            if derived != in_cert {
                let cs = user_cert["subject"]["callsign"].as_str().unwrap_or("?");
                return Err(format!(
                    "cert_devicekey 私钥与 cert_user（{cs}）证书公钥不匹配：\
                     导入的证书不是同一个人的一套，请 4 个证书文件整套一起换"
                ));
            }
        }
    }
    Ok(())
}

/// 身份状态巡检（启动/定时任务用）：
/// None = 未导入用户证书；Some(Err) = 有问题需提醒；Some(Ok) = 正常（附剩余天数）。
pub fn identity_status(certs_dir: &Path) -> Option<Result<String, String>> {
    if !certs_dir.join("cert_user.json").is_file() {
        return None;
    }
    if !certs_dir.join("cert_devicekey.json").is_file() {
        return Some(Err(
            "缺少 cert_devicekey.json（私钥），MQTT 认证无法签名".into()
        ));
    }
    if let Err(e) = validate_identity(certs_dir) {
        return Some(Err(e));
    }
    let user_cert = _load_cert(certs_dir, "cert_user.json").ok()?;
    let exp = user_cert["exp"].as_u64()? as i64;
    let days = (exp - protocol::now_ts()) / 86400;
    if days < 7 {
        Some(Err(format!("用户证书将在 {days} 天后过期，请尽快更新证书")))
    } else {
        Some(Ok(format!("证书有效，剩余 {days} 天")))
    }
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
        let pw = build_mqtt_password(
            &user_cert,
            &int_cert,
            "taIOylDLfECyOfG7PQFVO54jAGUbtMx7ztyYSQLUHGQ",
            &server,
            "user",
            Some(1786323504),
        )
        .unwrap();
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
