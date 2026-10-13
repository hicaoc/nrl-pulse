use sha2::{Digest, Sha256};

/// 跨平台稳定 48 位机器 ID（MAC 形态，用于 FMO 证书绑定）。
///
/// 派生算法（**绑定契约的一部分，发布后禁止修改**，否则已登记绑定全部失效）：
/// 1. seed = macOS `IOPlatformUUID`（ioreg IOPlatformExpertDevice）
///          / Linux `/etc/machine-id`（回退 `/var/lib/dbus/machine-id`）
///          / Windows `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid`
/// 2. id = SHA-256(seed) 前 6 字节
/// 3. id[0] = (id[0] | 0x02) & 0xFE（置本地管理位、清组播位，与单播 MAC 同形态）
pub fn stable_machine_id() -> Result<[u8; 6], String> {
    let seed = machine_seed()?;
    let digest = Sha256::digest(seed.as_bytes());
    let mut id = [0u8; 6];
    id.copy_from_slice(&digest[..6]);
    id[0] = (id[0] | 0x02) & 0xFE;
    Ok(id)
}

/// 12 位大写十六进制字符串；读取失败返回空串
pub fn stable_machine_id_string() -> String {
    stable_machine_id()
        .map(|b| hex::encode(b).to_uppercase())
        .unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn machine_seed() -> Result<String, String> {
    let out = std::process::Command::new("ioreg")
        .args(["-d2", "-c", "IOPlatformExpertDevice"])
        .output()
        .map_err(|e| format!("执行 ioreg 失败: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if line.contains("\"IOPlatformUUID\"") {
            if let Some(v) = line.split('"').nth(3) {
                let v = v.trim();
                if !v.is_empty() {
                    return Ok(v.to_string());
                }
            }
        }
    }
    Err("读取 IOPlatformUUID 失败".to_string())
}

#[cfg(target_os = "linux")]
fn machine_seed() -> Result<String, String> {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(content) = std::fs::read_to_string(path) {
            let content = content.trim();
            if !content.is_empty() {
                return Ok(content.to_string());
            }
        }
    }
    Err("读取 /etc/machine-id 失败".to_string())
}

#[cfg(target_os = "windows")]
fn machine_seed() -> Result<String, String> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};
    use winreg::RegKey;
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = hklm
        .open_subkey_with_flags("SOFTWARE\\Microsoft\\Cryptography", KEY_READ)
        .map_err(|e| format!("打开 Cryptography 注册表键失败: {e}"))?;
    let guid: String = key
        .get_value("MachineGuid")
        .map_err(|e| format!("读取 MachineGuid 失败: {e}"))?;
    let guid = guid.trim();
    if guid.is_empty() {
        return Err("MachineGuid 为空".to_string());
    }
    Ok(guid.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_stable_and_mac_shaped() {
        let a = stable_machine_id().expect("machine id");
        let b = stable_machine_id().expect("machine id again");
        assert_eq!(a, b);
        assert_eq!(a[0] & 0x02, 0x02, "locally administered bit set");
        assert_eq!(a[0] & 0x01, 0x00, "multicast bit cleared");
        assert_eq!(stable_machine_id_string().len(), 12);
    }
}
