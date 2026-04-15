use serde::Serialize;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSettings {
    pub input_device: String,
    pub output_device: String,
    pub sample_rate: u32,
    pub input_device_rate: u32,
    pub output_device_rate: u32,
    pub input_resampling: bool,
    pub output_resampling: bool,
    pub jitter_buffer_ms: u32,
    pub agc_enabled: bool,
    pub noise_suppression: bool,
    pub aec_enabled: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSnapshot {
    pub room_name: String,
    pub callsign: String,
    pub ssid: u8,
    pub active_speaker: String,
    pub active_speaker_ssid: u8,
    pub connection: String,
    pub packet_loss: f32,
    pub latency_ms: u32,
    pub jitter_ms: u32,
    pub uplink_kbps: f32,
    pub downlink_kbps: f32,
    pub rx_level: f32,
    pub tx_level: f32,
    pub rx_spectrum: Vec<f32>,
    pub tx_spectrum: Vec<f32>,
    pub is_transmitting: bool,
    pub is_monitoring: bool,
    pub queued_frames: u32,
    pub last_text_message: String,
    pub devices: DeviceSettings,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresenceItem {
    pub id: String,
    pub callsign: String,
    pub ssid: u8,
    pub role: String,
    pub state: String,
    pub signal: i32,
    pub last_seen: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineEvent {
    pub id: String,
    pub time: String,
    pub title: String,
    pub detail: String,
    pub tone: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessageEvent {
    pub id: String,
    pub side: String,
    pub text: String,
    pub meta: String,
    pub time: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waveform: Option<Vec<f32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeBootstrap {
    pub snapshot: SessionSnapshot,
    pub presence: Vec<PresenceItem>,
    pub timeline: Vec<TimelineEvent>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RealtimeAudioState {
    pub active_speaker: String,
    pub active_speaker_ssid: u8,
    pub rx_level: f32,
    pub tx_level: f32,
    pub rx_spectrum: Vec<f32>,
    pub tx_spectrum: Vec<f32>,
    pub queued_frames: u32,
    pub uplink_kbps: f32,
    pub downlink_kbps: f32,
    pub is_transmitting: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AtState {
    pub volume: u8,
    pub duck_mic: bool,
    pub duck_music: bool,
    pub duck_scale: u8,
    pub last_command: String,
}
