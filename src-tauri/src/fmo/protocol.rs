//! FMO-V4 私有协议共用原语：base64url / Ed25519 / JWT(HS256) / CBOR / 证书编解码。
//!
//! 移植自 open-fmo/sim-rust 的 protocol.rs。

use base64::Engine;
use ed25519_dalek::Signer;
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------- base64url

pub fn b64url_encode(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

pub fn b64url_decode(text: &str) -> Result<Vec<u8>, base64::DecodeError> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(text.trim_end_matches('='))
}

pub fn b64_encode(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

pub fn b64_decode(text: &str) -> Result<Vec<u8>, base64::DecodeError> {
    base64::engine::general_purpose::STANDARD.decode(text)
}

/// 兼容多种编码的 seed/密钥解码：hex、标准 base64、base64url（无 padding）。
pub fn decode_seed(input: &str) -> Option<Vec<u8>> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    let is_hex = !s.is_empty() && s.len() % 2 == 0 && s.chars().all(|c| c.is_ascii_hexdigit());
    if is_hex {
        if let Ok(v) = hex::decode(s) {
            return Some(v);
        }
    }
    if let Ok(v) = base64::engine::general_purpose::STANDARD.decode(s) {
        return Some(v);
    }
    if let Ok(v) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s.trim_end_matches('='))
    {
        return Some(v);
    }
    None
}

// ---------------------------------------------------------------- Ed25519

pub fn generate_keypair() -> serde_json::Map<String, serde_json::Value> {
    let mut csprng = rand::rngs::OsRng;
    let kp = ed25519_dalek::SigningKey::generate(&mut csprng);
    let mut m = serde_json::Map::new();
    m.insert(
        "seed".into(),
        serde_json::Value::String(b64_encode(&kp.to_bytes())),
    );
    m.insert(
        "pubKey".into(),
        serde_json::Value::String(b64_encode(&kp.verifying_key().to_bytes())),
    );
    m
}

pub fn sign(seed_b64: &str, message: &[u8]) -> Result<Vec<u8>, String> {
    let seed = decode_seed(seed_b64)
        .ok_or_else(|| "seed 编码无法识别（需 base64/base64url/hex）".to_string())?;
    let kp = ed25519_dalek::SigningKey::from_bytes(
        seed.as_slice()
            .try_into()
            .map_err(|_| "seed 长度不是 32B")?,
    );
    Ok(kp.sign(message).to_bytes().to_vec())
}

/// 从 seed 推导 Ed25519 公钥（校验 devicekey 私钥与 user 证书是否一套用）。
pub fn pubkey_from_seed(seed_b64: &str) -> Option<Vec<u8>> {
    let seed = decode_seed(seed_b64)?;
    let kp = ed25519_dalek::SigningKey::from_bytes(seed.as_slice().try_into().ok()?);
    Some(kp.verifying_key().to_bytes().to_vec())
}

pub fn verify(pubkey_b64: &str, message: &[u8], signature: &[u8]) -> bool {
    let Some(pk) = decode_seed(pubkey_b64) else {
        return false;
    };
    let Ok(arr) = <[u8; 32]>::try_from(pk.as_slice()) else {
        return false;
    };
    let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&arr) else {
        return false;
    };
    let Ok(sig) = ed25519_dalek::Signature::from_slice(signature) else {
        return false;
    };
    vk.verify_strict(message, &sig).is_ok()
}

// ---------------------------------------------------------------- JWT(HS256)

use hmac::{Hmac, Mac};

pub fn jwt_sign(
    payload: &serde_json::Map<String, serde_json::Value>,
    key: &[u8],
) -> Result<String, String> {
    let mut hdr = serde_json::Map::new();
    hdr.insert("alg".into(), serde_json::Value::String("HS256".into()));
    hdr.insert("typ".into(), serde_json::Value::String("JWT".into()));
    let seg_h = b64url_encode(&serde_json::to_vec(&hdr).map_err(|e| e.to_string())?);
    let seg_p = b64url_encode(&serde_json::to_vec(payload).map_err(|e| e.to_string())?);
    let signing_input = format!("{seg_h}.{seg_p}");
    let mut mac = <Hmac<Sha256>>::new_from_slice(key).map_err(|e| e.to_string())?;
    mac.update(signing_input.as_bytes());
    let sig = mac.finalize().into_bytes();
    Ok(format!("{seg_h}.{seg_p}.{}", b64url_encode(&sig)))
}

pub fn jwt_decode(
    token: &str,
    key: Option<&[u8]>,
) -> Result<(serde_json::Value, serde_json::Value, bool), String> {
    let mut it = token.split('.');
    let (seg_h, seg_p, seg_s) = (it.next(), it.next(), it.next());
    let (Some(seg_h), Some(seg_p), Some(seg_s)) = (seg_h, seg_p, seg_s) else {
        return Err("JWT 段数不足".into());
    };
    let header: serde_json::Value =
        serde_json::from_slice(&b64url_decode(seg_h).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let payload: serde_json::Value =
        serde_json::from_slice(&b64url_decode(seg_p).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let mut ok = false;
    if let Some(k) = key {
        let mut mac = <Hmac<Sha256>>::new_from_slice(k).map_err(|e| e.to_string())?;
        mac.update(format!("{seg_h}.{seg_p}").as_bytes());
        let expect = mac.finalize().into_bytes();
        let got = b64url_decode(seg_s).map_err(|e| e.to_string())?;
        ok = expect.as_slice() == got.as_slice();
    }
    Ok((header, payload, ok))
}

pub fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// CBOR 编码（ciborium 包装）。
pub fn cbor_encode<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    ciborium::ser::into_writer(value, &mut buf).map_err(|e| e.to_string())?;
    Ok(buf)
}

// ---------------------------------------------------------------- FMO 证书（CBOR 紧凑格式）

// CA 根 Ed25519 公钥（固件内嵌根证书 JSON 模板 @0x15686 逆向确认）
pub const CA_ROOT_PUBKEY_B64URL: &str = "DCeeVS320f36ToVP2eOADVN-Q0LzpMYmiVkmNYzuysY";

#[derive(Debug, Clone)]
pub struct FmoCert {
    pub magic: String,
    pub version: u64,
    pub cert_type: String,
    pub alg: u64,
    pub callsign: String,
    pub uid: u64,
    pub pubkey_hex: String,
    pub iat: u64,
    pub exp: u64,
    pub ca_sig_hex: String,
    pub raw: Vec<u8>,
}

impl FmoCert {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "magic": self.magic, "version": self.version, "type": self.cert_type,
            "alg": self.alg, "callsign": self.callsign, "uid": self.uid,
            "pubkey_hex": self.pubkey_hex, "iat": self.iat, "exp": self.exp,
            "ca_sig_hex": self.ca_sig_hex,
        })
    }

    /// 从 APRS 广播解析出的 CERT dict 算指纹（beacon_cert_fingerprint 语义）。
    pub fn beacon_fingerprint(&self) -> Vec<u8> {
        let tbs = vec![
            CborValue::Text("FMO".into()),
            CborValue::UInt(4),
            CborValue::Text("userCert".into()),
            CborValue::UInt(self.alg),
            CborValue::Text(self.callsign.clone()),
            CborValue::UInt(self.uid),
            CborValue::Bytes(hex_to_bytes(&self.pubkey_hex)),
            CborValue::UInt(self.iat),
            CborValue::UInt(self.exp),
        ];
        let mut hasher = Sha256::new();
        hasher.update(cbor_tbs(&tbs));
        hasher.finalize().to_vec()
    }
}

fn hex_to_bytes(s: &str) -> Vec<u8> {
    if let Ok(v) = hex::decode(s) {
        v
    } else {
        Vec::new()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CborValue {
    UInt(u64),
    NegInt(i64),
    Bytes(Vec<u8>),
    Text(String),
    /// CBOR text（major 3）但内容为原始字节（如 GBK 编码的名称，固件按 std::string 直接写入）
    TextBytes(Vec<u8>),
    Array(Vec<CborValue>),
}

fn cbor_head(b: u8) -> (u8, u64) {
    let major = b >> 5;
    let info = (b & 0x1f) as u64;
    (major, info)
}

fn parse_cbor(buf: &[u8], pos: &mut usize) -> Option<CborValue> {
    let b = *buf.get(*pos)?;
    *pos += 1;
    let (major, mut info) = cbor_head(b);
    if info == 24 {
        info = *buf.get(*pos)? as u64;
        *pos += 1;
    } else if info == 25 {
        let hi = *buf.get(*pos)? as u64;
        let lo = *buf.get(*pos + 1)? as u64;
        info = (hi << 8) | lo;
        *pos += 2;
    } else if info == 26 {
        info = u32::from_be_bytes([
            *buf.get(*pos)?,
            *buf.get(*pos + 1)?,
            *buf.get(*pos + 2)?,
            *buf.get(*pos + 3)?,
        ]) as u64;
        *pos += 4;
    } else if info == 27 {
        info = u64::from_be_bytes([
            *buf.get(*pos)?,
            *buf.get(*pos + 1)?,
            *buf.get(*pos + 2)?,
            *buf.get(*pos + 3)?,
            *buf.get(*pos + 4)?,
            *buf.get(*pos + 5)?,
            *buf.get(*pos + 6)?,
            *buf.get(*pos + 7)?,
        ]);
        *pos += 8;
    } else if info == 31 {
        return None;
    }
    match major {
        0 => Some(CborValue::UInt(info)),
        1 => Some(CborValue::NegInt(-1 - info as i64)),
        2 => {
            let end = *pos + info as usize;
            if end > buf.len() {
                return None;
            }
            let v = buf[*pos..end].to_vec();
            *pos = end;
            Some(CborValue::Bytes(v))
        }
        3 => {
            let end = *pos + info as usize;
            if end > buf.len() {
                return None;
            }
            let s = std::str::from_utf8(&buf[*pos..end]).ok()?.to_string();
            *pos = end;
            Some(CborValue::Text(s))
        }
        4 => {
            let mut arr = Vec::with_capacity(info as usize);
            for _ in 0..info {
                arr.push(parse_cbor(buf, pos)?);
            }
            Some(CborValue::Array(arr))
        }
        _ => None,
    }
}

pub fn cbor_decode(buf: &[u8]) -> Option<CborValue> {
    let mut pos = 0usize;
    parse_cbor(buf, &mut pos)
}

fn cbor_head_write(major: u8, value: u64, out: &mut Vec<u8>) {
    let info = if value < 24 {
        value as u8
    } else if value <= 0xff {
        24u8
    } else if value <= 0xffff {
        25u8
    } else if value <= 0xffff_ffff {
        26u8
    } else {
        27u8
    };
    out.push((major << 5) | info);
    match info {
        24 => out.push(value as u8),
        25 => out.extend_from_slice(&(value as u16).to_be_bytes()),
        26 => out.extend_from_slice(&(value as u32).to_be_bytes()),
        27 => out.extend_from_slice(&value.to_be_bytes()),
        _ => {}
    }
}

pub fn cbor_encode_value(v: &CborValue) -> Vec<u8> {
    let mut out = Vec::new();
    cbor_write_value(v, &mut out);
    out
}

fn cbor_write_value(v: &CborValue, out: &mut Vec<u8>) {
    match v {
        CborValue::UInt(n) => cbor_head_write(0, *n, out),
        CborValue::NegInt(n) => cbor_head_write(1, (-1 - *n) as u64, out),
        CborValue::Bytes(b) => {
            cbor_head_write(2, b.len() as u64, out);
            out.extend_from_slice(b);
        }
        CborValue::Text(s) => {
            cbor_head_write(3, s.len() as u64, out);
            out.extend_from_slice(s.as_bytes());
        }
        CborValue::TextBytes(b) => {
            cbor_head_write(3, b.len() as u64, out);
            out.extend_from_slice(b);
        }
        CborValue::Array(a) => {
            cbor_head_write(4, a.len() as u64, out);
            for item in a {
                cbor_write_value(item, out);
            }
        }
    }
}

pub fn cbor_tbs(array: &[CborValue]) -> Vec<u8> {
    cbor_encode_value(&CborValue::Array(array.to_vec()))
}

pub fn decode_fmo_cert(b64url_text: &str) -> Option<FmoCert> {
    let raw = b64url_decode(b64url_text).ok()?;
    let val = cbor_decode(&raw)?;
    let CborValue::Array(arr) = val else {
        return None;
    };
    if arr.len() != 10 {
        return None;
    }
    let CborValue::Text(magic) = &arr[0] else {
        return None;
    };
    if magic != "FMO" {
        return None;
    }
    let CborValue::UInt(version) = arr[1] else {
        return None;
    };
    let CborValue::Text(cert_type) = &arr[2] else {
        return None;
    };
    let CborValue::UInt(alg) = arr[3] else {
        return None;
    };
    let CborValue::Text(callsign) = &arr[4] else {
        return None;
    };
    let CborValue::UInt(uid) = arr[5] else {
        return None;
    };
    let CborValue::Bytes(pubkey) = &arr[6] else {
        return None;
    };
    let CborValue::UInt(iat) = arr[7] else {
        return None;
    };
    let CborValue::UInt(exp) = arr[8] else {
        return None;
    };
    let CborValue::Bytes(ca_sig) = &arr[9] else {
        return None;
    };
    Some(FmoCert {
        magic: magic.clone(),
        version,
        cert_type: cert_type.clone(),
        alg,
        callsign: callsign.clone(),
        uid,
        pubkey_hex: hex::encode(pubkey),
        iat,
        exp,
        ca_sig_hex: hex::encode(ca_sig),
        raw,
    })
}

pub fn verify_fmo_cert(cert: &FmoCert) -> bool {
    let Ok(vk_b) = b64url_decode(CA_ROOT_PUBKEY_B64URL) else {
        return false;
    };
    let Ok(arr) = <[u8; 32]>::try_from(vk_b.as_slice()) else {
        return false;
    };
    let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&arr) else {
        return false;
    };
    let Ok(sig_b) = hex::decode(&cert.ca_sig_hex) else {
        return false;
    };
    let Ok(sig) = ed25519_dalek::Signature::from_slice(&sig_b) else {
        return false;
    };

    let tbs = vec![
        CborValue::Text("FMO".into()),
        CborValue::UInt(4),
        CborValue::Text("userCert".into()),
        CborValue::UInt(cert.alg),
        CborValue::Text(cert.callsign.clone()),
        CborValue::UInt(cert.uid),
        CborValue::Bytes(hex_to_bytes(&cert.pubkey_hex)),
        CborValue::UInt(cert.iat),
        CborValue::UInt(cert.exp),
    ];
    let payload1 = cbor_tbs(&tbs);
    if vk.verify_strict(&payload1, &sig).is_ok() {
        return true;
    }
    if cert.raw.len() > 66 {
        if vk
            .verify_strict(&cert.raw[..cert.raw.len() - 66], &sig)
            .is_ok()
        {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------- 证书指纹

pub fn user_cert_tbs(user_cert: &serde_json::Value) -> Vec<CborValue> {
    let subject = &user_cert["subject"];
    let pubkey = decode_seed(subject["publicKey"].as_str().unwrap_or("")).unwrap_or_default();
    vec![
        CborValue::Text("FMO".into()),
        CborValue::UInt(4),
        CborValue::Text("userCert".into()),
        CborValue::UInt(user_cert["issuerSn"].as_u64().unwrap_or(0)),
        CborValue::Text(subject["callsign"].as_str().unwrap_or("").to_string()),
        CborValue::UInt(subject["uid"].as_u64().unwrap_or(0)),
        CborValue::Bytes(pubkey),
        CborValue::UInt(user_cert["iat"].as_u64().unwrap_or(0)),
        CborValue::UInt(user_cert["exp"].as_u64().unwrap_or(0)),
    ]
}

pub fn cert_fingerprint(user_cert: &serde_json::Value) -> Vec<u8> {
    let tbs = user_cert_tbs(user_cert);
    let encoded = cbor_tbs(&tbs);
    let mut hasher = Sha256::new();
    hasher.update(&encoded);
    hasher.finalize().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64url_roundtrip() {
        assert_eq!(
            b64url_decode(&b64url_encode(b"hello world")).unwrap(),
            b"hello world"
        );
        assert_eq!(b64url_encode(b"\xfb\xff"), "-_8");
    }

    #[test]
    fn ed25519_sign_verify() {
        let kp = generate_keypair();
        let msg = b"test message";
        let sig = sign(kp["seed"].as_str().unwrap(), msg).unwrap();
        assert!(verify(kp["pubKey"].as_str().unwrap(), msg, &sig));
        assert!(!verify(kp["pubKey"].as_str().unwrap(), b"other", &sig));
    }

    #[test]
    fn jwt_roundtrip() {
        let mut claims = serde_json::Map::new();
        claims.insert("iat".into(), serde_json::Value::from(now_ts()));
        let tok = jwt_sign(&claims, b"secret").unwrap();
        let (_, payload, ok) = jwt_decode(&tok, Some(b"secret")).unwrap();
        assert!(ok);
        assert_eq!(payload["iat"], claims["iat"]);
        let (_, _, bad) = jwt_decode(&tok, Some(b"wrong")).unwrap();
        assert!(!bad);
    }

    #[test]
    fn decode_real_fmo_cert() {
        let cert = "imNGTU8EaHVzZXJDZXJ0GQPpZkJENkpEVRkCaFggIFRhZW0zYdC3wOanssyDfaxSdSYTX_V_tARfaz1zKN4aajEeqBpsElIoWED2Z-y8Lfwu8M0LaDtr9xR55ODnyZ5jtBnwyG4IAXdbCTH4NkOXvYlvOUTr8ANQte1t4ApL247Ducx2b1HBPRMK";
        let c = decode_fmo_cert(cert).expect("应能解码实网 CERT");
        assert_eq!(c.magic, "FMO");
        assert_eq!(c.version, 4);
        assert_eq!(c.cert_type, "userCert");
        assert_eq!(c.alg, 1001);
        assert_eq!(c.callsign, "BD6JDU");
        assert_eq!(c.uid, 0x0268);
        assert_eq!(c.pubkey_hex.len(), 64);
        assert_eq!(c.ca_sig_hex.len(), 128);
    }

    #[test]
    fn cbor_tbs_matches_python_cbor2() {
        let pubkey: Vec<u8> = (0u8..32).collect();
        let tbs = vec![
            CborValue::Text("FMO".into()),
            CborValue::UInt(4),
            CborValue::Text("userCert".into()),
            CborValue::UInt(1001),
            CborValue::Text("BG9JYT".into()),
            CborValue::UInt(447),
            CborValue::Bytes(pubkey),
            CborValue::UInt(1000),
            CborValue::UInt(31537000),
        ];
        let encoded = cbor_tbs(&tbs);
        let expect = "8963464d4f046875736572436572741903e9664247394a59541901bf5820\
000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\
1903e81a01e13768";
        assert_eq!(hex::encode(&encoded), expect);
    }
}
