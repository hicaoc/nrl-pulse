use std::fmt;

pub const NRL_HEADER_LEN: usize = 48;
pub const NRL_DEVICE_MODE: u8 = 110;
pub const NRL_CPUID_UNUSED: [u8; 4] = [0, 0, 0, 0];

#[derive(Clone, Debug)]
pub struct NrlPacket {
    pub version: [u8; 4],
    pub length: u16,
    pub cpuid: [u8; 4],
    pub packet_type: u8,
    pub status: u8,
    pub count: u16,
    pub callsign: [u8; 6],
    pub ssid: u8,
    pub dev_mode: u8,
    pub data: Vec<u8>,
}

impl NrlPacket {
    pub fn encode(&self) -> Vec<u8> {
        let total = NRL_HEADER_LEN + self.data.len();
        let mut packet = vec![0_u8; total];
        packet[0..4].copy_from_slice(&self.version);
        packet[4..6].copy_from_slice(&(total as u16).to_be_bytes());
        packet[6..10].copy_from_slice(&self.cpuid);
        packet[20] = self.packet_type;
        packet[21] = self.status;
        packet[22..24].copy_from_slice(&self.count.to_be_bytes());
        packet[24..30].copy_from_slice(&self.callsign);
        packet[30] = self.ssid;
        packet[31] = self.dev_mode;
        packet[48..].copy_from_slice(&self.data);
        packet
    }

    pub fn decode(input: &[u8]) -> Result<Self, String> {
        if input.len() < NRL_HEADER_LEN {
            return Err("packet too short".into());
        }

        let version: [u8; 4] = input[0..4]
            .try_into()
            .map_err(|_| "invalid version header".to_string())?;
        if &version != b"NRL2" {
            return Err("invalid NRL2 magic".into());
        }

        let length = u16::from_be_bytes([input[4], input[5]]);
        let cpuid = input[6..10]
            .try_into()
            .map_err(|_| "invalid cpuid".to_string())?;
        let packet_type = input[20];
        let status = input[21];
        let count = u16::from_be_bytes([input[22], input[23]]);
        let callsign = input[24..30]
            .try_into()
            .map_err(|_| "invalid callsign".to_string())?;

        Ok(Self {
            version,
            length,
            cpuid,
            packet_type,
            status,
            count,
            callsign,
            ssid: input[30],
            dev_mode: input[31],
            data: input[48..].to_vec(),
        })
    }

    pub fn voice_frame(callsign: &str, ssid: u8, frame: Vec<u8>) -> Self {
        let mut cs = [0_u8; 6];
        for (slot, byte) in cs.iter_mut().zip(callsign.as_bytes().iter().copied()) {
            *slot = byte;
        }

        Self {
            version: *b"NRL2",
            length: (NRL_HEADER_LEN + frame.len()) as u16,
            cpuid: NRL_CPUID_UNUSED,
            packet_type: 1,
            status: 1,
            count: 0,
            callsign: cs,
            ssid,
            dev_mode: NRL_DEVICE_MODE,
            data: frame,
        }
    }

    pub fn heartbeat(callsign: &str, ssid: u8) -> Self {
        Self::base_packet(callsign, ssid, 2, Vec::new())
    }

    pub fn text_message(callsign: &str, ssid: u8, data: Vec<u8>) -> Self {
        Self::base_packet(callsign, ssid, 5, data)
    }

    pub fn at_message(callsign: &str, ssid: u8, data: Vec<u8>) -> Self {
        Self::base_packet(callsign, ssid, 11, data)
    }

    pub fn callsign_string(&self) -> String {
        String::from_utf8_lossy(&self.callsign)
            .trim_matches(char::from(0))
            .trim_matches(char::from(13))
            .to_string()
    }

    fn base_packet(callsign: &str, ssid: u8, packet_type: u8, data: Vec<u8>) -> Self {
        let mut cs = [0_u8; 6];
        for (slot, byte) in cs.iter_mut().zip(callsign.as_bytes().iter().copied()) {
            *slot = byte;
        }

        Self {
            version: *b"NRL2",
            length: (NRL_HEADER_LEN + data.len()) as u16,
            cpuid: NRL_CPUID_UNUSED,
            packet_type,
            status: 1,
            count: 0,
            callsign: cs,
            ssid,
            dev_mode: NRL_DEVICE_MODE,
            data,
        }
    }
}

impl fmt::Display for NrlPacket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let callsign = self.callsign_string();
        write!(
            f,
            "NRL2 type={} callsign={}-{} len={} payload={}",
            self.packet_type,
            callsign,
            self.ssid,
            self.length,
            self.data.len()
        )
    }
}
