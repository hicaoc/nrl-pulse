use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RuntimeConfig {
    pub server: String,
    pub port: u16,
    pub server_name: String,
    pub api_base: String,
    pub auth_token: String,
    pub login_username: String,
    pub callsign: String,
    pub ssid: u8,
    pub room_name: String,
    pub current_group_id: i32,
    pub volume: f32,
    pub ptt_key: String,
    pub voice_save_path: String,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            server: "127.0.0.1".into(),
            port: 10024,
            server_name: "Local".into(),
            api_base: String::new(),
            auth_token: String::new(),
            login_username: String::new(),
            callsign: "B1NRL".into(),
            ssid: 110,
            room_name: "NRL East Hub".into(),
            current_group_id: 0,
            volume: 1.0,
            ptt_key: "Space".into(),
            voice_save_path: String::new(),
        }
    }
}

pub fn load_or_default(app: &AppHandle) -> RuntimeConfig {
    let path = config_path(app);
    let mut config = std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<RuntimeConfig>(&raw).ok())
        .unwrap_or_default();
    config.ssid = 110;
    if config.ptt_key.trim().is_empty() {
        config.ptt_key = "Space".into();
    }
    config
}

pub fn save(app: &AppHandle, config: &RuntimeConfig) -> Result<(), String> {
    let path = config_path(app);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let mut normalized = config.clone();
    normalized.ssid = 110;
    if normalized.ptt_key.trim().is_empty() {
        normalized.ptt_key = "Space".into();
    }
    let raw = serde_json::to_string_pretty(&normalized).map_err(|err| err.to_string())?;
    std::fs::write(path, raw).map_err(|err| err.to_string())
}

fn config_path(app: &AppHandle) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("runtime.json")
}
