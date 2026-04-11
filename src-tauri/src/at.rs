#[derive(Clone, Debug)]
pub struct AtCommand {
    pub command: String,
    pub value: String,
}

pub fn decode_at(data: &[u8]) -> Option<AtCommand> {
    if data.len() < 2 || data[0] != 0x01 {
        return None;
    }

    let text = String::from_utf8_lossy(&data[1..]);
    let (command, value) = text.split_once('=')?;
    Some(AtCommand {
        command: command.trim().to_string(),
        value: value.trim().trim_end_matches("\r\n").to_string(),
    })
}

pub fn encode_at(lines: &[String]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(64);
    payload.push(0x02);
    payload.extend_from_slice(b"NRLNANNY V2.0\r\n");
    payload.extend_from_slice(lines.join("\r\n").as_bytes());
    payload.extend_from_slice(b"\r\n");
    payload
}
