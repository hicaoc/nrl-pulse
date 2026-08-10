//! FMO 证书库：手工导入解密后的证书 JSON，持久化到 certs 目录。

use serde_json::json;
use sha2::Digest;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

fn canon(obj: &serde_json::Value) -> String {
    fn helper(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(m) => {
                let mut sorted: Vec<(String, serde_json::Value)> = m.iter()
                    .map(|(k, val)| (k.clone(), helper(val))).collect();
                sorted.sort_by(|a, b| a.0.cmp(&b.0));
                serde_json::Value::Object(sorted.into_iter().collect())
            }
            serde_json::Value::Array(a) => {
                serde_json::Value::Array(a.iter().map(helper).collect())
            }
            other => other.clone(),
        }
    }
    serde_json::to_string(&helper(obj)).unwrap_or_default()
}

fn fingerprint(obj: &serde_json::Value) -> String {
    let digest = sha2::Sha256::digest(canon(obj).as_bytes());
    hex::encode(&digest[..8])
}

fn now_ts() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn subject_info(obj: &serde_json::Value) -> String {
    if !obj.is_object() {
        return String::new();
    }
    for key in ["subject", "callsign", "commonName", "name", "uid"] {
        if let Some(v) = obj.get(key) {
            return format!("{key}={v}");
        }
    }
    if let Some(pubk) = obj.get("pubKey").and_then(|v| v.as_str()) {
        return format!("pubKey={}…", &pubk[..pubk.len().min(16)]);
    }
    if let Some(keys) = obj.as_object() {
        let ks: Vec<String> = keys.keys().take(5).cloned().collect();
        return format!("keys={}", ks.join(","));
    }
    String::new()
}

fn save_index(path: &std::path::Path, index: &[serde_json::Value]) {
    if let Ok(text) = serde_json::to_string_pretty(index) {
        std::fs::write(path, text).ok();
    }
}

pub struct CertStore {
    pub dir: PathBuf,
    pub index_path: PathBuf,
    pub index: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl CertStore {
    pub fn new(cert_dir: PathBuf) -> Self {
        std::fs::create_dir_all(&cert_dir).ok();
        let index_path = cert_dir.join("index.json");
        let mut index: Vec<serde_json::Value> = Vec::new();
        if let Ok(text) = std::fs::read_to_string(&index_path) {
            if let Ok(v) = serde_json::from_str::<Vec<serde_json::Value>>(&text) {
                index = v;
            }
        }
        let known: std::collections::HashSet<String> = index.iter()
            .filter_map(|e| e.get("file").and_then(|f| f.as_str()).map(|s| s.to_string()))
            .collect();
        let mut changed = false;
        if let Ok(entries) = std::fs::read_dir(&cert_dir) {
            let mut files: Vec<_> = entries.filter_map(|e| e.ok()).collect();
            files.sort_by_key(|e| e.file_name());
            for e in files {
                let fname = e.file_name().to_string_lossy().into_owned();
                if !fname.ends_with(".json") || fname == "index.json" || known.contains(&fname) {
                    continue;
                }
                let path = e.path();
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                let Ok(obj) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
                let fp = fingerprint(&obj);
                if index.iter().any(|en| en["fingerprint"] == fp) {
                    continue;
                }
                index.push(json!({
                    "name": path.file_stem().and_then(|s| s.to_str()).unwrap_or(""),
                    "fingerprint": fp, "source": "autoload",
                    "info": subject_info(&obj), "valid": obj.is_object(),
                    "imported_at": now_ts(),
                    "file": fname,
                }));
                changed = true;
            }
        }
        if changed {
            save_index(&index_path, &index);
        }
        Self { dir: cert_dir, index_path, index: Arc::new(Mutex::new(index)) }
    }

    pub async fn list(&self) -> Vec<serde_json::Value> {
        self.index.lock().await.clone()
    }

    /// 手动导入解密后的证书 JSON。name 为 cert_user/cert_int/cert_root/cert_devicekey 时
    /// 同时写固定文件名（供身份读取）。
    pub async fn import_json(&self, name: &str, obj: serde_json::Value,
                             source: &str) -> serde_json::Value {
        let identity_names = ["cert_user", "cert_devicekey", "cert_int", "cert_root"];
        if identity_names.contains(&name) {
            let p = self.dir.join(format!("{name}.json"));
            if let Ok(text) = serde_json::to_string_pretty(&obj) {
                std::fs::write(&p, text).ok();
            }
        }
        self.store_async(name, obj, source).await
    }

    async fn store_async(&self, name: &str, obj: serde_json::Value, source: &str)
        -> serde_json::Value {
        let fp = fingerprint(&obj);
        let entry = json!({
            "name": name, "fingerprint": fp, "source": source,
            "info": subject_info(&obj), "valid": obj.is_object(),
            "imported_at": now_ts(),
            "file": format!("{fp}.json"),
        });
        let file = self.dir.join(format!("{fp}.json"));
        if let Ok(text) = serde_json::to_string_pretty(&obj) {
            std::fs::write(&file, text).ok();
        }
        let mut index = self.index.lock().await;
        index.retain(|e| e["fingerprint"] != fp);
        index.push(entry.clone());
        save_index(&self.index_path, &index);
        entry
    }
}
