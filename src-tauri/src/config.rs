use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SerialTunnelConfig {
    pub mode: String,
    pub auto_start: bool,
    pub port_name: String,
    pub baud_rate: u32,
    pub data_bits: u8,
    pub parity: String,
    pub stop_bits: String,
    pub flow_control: String,
}

impl Default for SerialTunnelConfig {
    fn default() -> Self {
        Self {
            mode: "physical".into(),
            auto_start: false,
            port_name: String::new(),
            baud_rate: 115_200,
            data_bits: 8,
            parity: "none".into(),
            stop_bits: "one".into(),
            flow_control: "none".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RuntimeConfig {
    /// "nrl" | "fmo"：当前协议
    pub protocol: String,
    /// "alaw" | "opus"：NRL 语音编码（type 8 = opus）
    pub voice_codec: String,
    /// "adpcm" | "opus"：FMO 语音编码
    pub fmo_voice_mode: String,
    /// FMO 独立呼号（为空时从证书读取）
    pub fmo_callsign: String,
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
    pub serial_tunnel: SerialTunnelConfig,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            protocol: "nrl".into(),
            voice_codec: "alaw".into(),
            fmo_voice_mode: "opus".into(),
            fmo_callsign: String::new(),
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
            serial_tunnel: SerialTunnelConfig::default(),
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
    normalize_config(&mut config);
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
    normalize_config(&mut normalized);
    let raw = serde_json::to_string_pretty(&normalized).map_err(|err| err.to_string())?;
    std::fs::write(path, raw).map_err(|err| err.to_string())
}

fn normalize_config(config: &mut RuntimeConfig) {
    if config.protocol != "fmo" {
        config.protocol = "nrl".into();
    }
    if config.voice_codec != "opus" {
        config.voice_codec = "alaw".into();
    }
    // UI 已不再提供选择：默认 Opus 发射；adpcm 仅保留为配置文件手动可选
    if config.fmo_voice_mode != "adpcm" {
        config.fmo_voice_mode = "opus".into();
    }
    normalize_serial_tunnel(&mut config.serial_tunnel);
}

fn normalize_serial_tunnel(config: &mut SerialTunnelConfig) {
    config.mode = "physical".into();
    config.auto_start = false;
    config.port_name = config.port_name.trim().to_string();
    #[cfg(target_os = "windows")]
    {
        config.port_name = config.port_name.to_uppercase();
    }
    #[cfg(target_os = "windows")]
    if !config.port_name.starts_with("COM")
        && config.port_name.chars().all(|ch| ch.is_ascii_digit())
    {
        config.port_name = format!("COM{}", config.port_name);
    }
    if config.baud_rate == 0 {
        config.baud_rate = SerialTunnelConfig::default().baud_rate;
    }
    if !matches!(config.data_bits, 5 | 6 | 7 | 8) {
        config.data_bits = 8;
    }
    if !matches!(config.parity.as_str(), "none" | "odd" | "even") {
        config.parity = "none".into();
    }
    if !matches!(config.stop_bits.as_str(), "one" | "two") {
        config.stop_bits = "one".into();
    }
    if !matches!(
        config.flow_control.as_str(),
        "none" | "software" | "hardware"
    ) {
        config.flow_control = "none".into();
    }
}

fn config_path(app: &AppHandle) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("runtime.json")
}
