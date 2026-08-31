mod at;
mod audio;
#[cfg(target_os = "macos")]
mod audio_aec_mac;
#[cfg(target_os = "windows")]
mod audio_aec_win;
mod config;
mod fmo;
mod g711;
mod models;
mod nrl;
mod opus;
mod platform;
mod runtime;
mod serial_tunnel;
mod udp;

use config::{RuntimeConfig, SerialTunnelConfig};
use models::{RuntimeBootstrap, SerialTunnelSnapshot, SessionSnapshot};
use platform::{
    GroupSnapshot, LoginBootstrap, PlatformDevice, PlatformRegisterPayload, PlatformRegisterResult,
    PlatformServer,
};
use runtime::RuntimeState;
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

#[tauri::command]
fn frontend_log(window: tauri::Window, msg: String) {
    eprintln!("[FE:{}] {}", window.label(), msg);
}

#[tauri::command]
async fn bootstrap_runtime(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, RuntimeState>,
) -> Result<RuntimeBootstrap, String> {
    let config = config::load_or_default(&app);
    state.apply_config(config.clone()).await;
    if window.label() == "main"
        && config.serial_tunnel.auto_start
        && !config.serial_tunnel.port_name.trim().is_empty()
    {
        let _ = state
            .start_serial_tunnel(config.serial_tunnel.clone())
            .await;
    }
    Ok(state.bootstrap().await)
}

fn broadcast_snapshot(app: &tauri::AppHandle, snapshot: &SessionSnapshot) {
    let _ = app.emit("runtime://snapshot", snapshot.clone());
}

#[tauri::command]
async fn connect_session(
    app: tauri::AppHandle,
    state: tauri::State<'_, RuntimeState>,
) -> Result<SessionSnapshot, String> {
    let config = config::load_or_default(&app);
    let snapshot = state.connect(config).await;
    broadcast_snapshot(&app, &snapshot);
    Ok(snapshot)
}

#[tauri::command]
async fn disconnect_session(
    app: tauri::AppHandle,
    state: tauri::State<'_, RuntimeState>,
) -> Result<SessionSnapshot, String> {
    let snapshot = state.disconnect().await;
    broadcast_snapshot(&app, &snapshot);
    Ok(snapshot)
}

#[tauri::command]
async fn toggle_transmit(
    app: tauri::AppHandle,
    state: tauri::State<'_, RuntimeState>,
) -> Result<SessionSnapshot, String> {
    let snapshot = state.toggle_transmit().await;
    broadcast_snapshot(&app, &snapshot);
    Ok(snapshot)
}

#[tauri::command]
async fn set_transmit(
    app: tauri::AppHandle,
    state: tauri::State<'_, RuntimeState>,
    enabled: bool,
) -> Result<SessionSnapshot, String> {
    let snapshot = state.set_transmit(enabled).await;
    broadcast_snapshot(&app, &snapshot);
    Ok(snapshot)
}

#[tauri::command]
async fn set_transmit_proto(
    app: tauri::AppHandle,
    state: tauri::State<'_, RuntimeState>,
    protocol: String,
    enabled: bool,
) -> Result<SessionSnapshot, String> {
    let snapshot = state.set_transmit_proto(&protocol, enabled).await;
    broadcast_snapshot(&app, &snapshot);
    Ok(snapshot)
}

#[tauri::command]
async fn toggle_monitor(
    app: tauri::AppHandle,
    state: tauri::State<'_, RuntimeState>,
) -> Result<SessionSnapshot, String> {
    let snapshot = state.toggle_monitor().await;
    broadcast_snapshot(&app, &snapshot);
    Ok(snapshot)
}

#[tauri::command]
async fn cycle_bridge_mode(
    app: tauri::AppHandle,
    state: tauri::State<'_, RuntimeState>,
) -> Result<SessionSnapshot, String> {
    let snapshot = state.cycle_bridge_mode().await;
    broadcast_snapshot(&app, &snapshot);
    Ok(snapshot)
}

#[tauri::command]
async fn update_jitter_buffer(
    state: tauri::State<'_, RuntimeState>,
    value: u32,
) -> Result<SessionSnapshot, String> {
    Ok(state.update_jitter_buffer(value).await)
}

#[tauri::command]
async fn send_text_message(
    app: tauri::AppHandle,
    state: tauri::State<'_, RuntimeState>,
    message: String,
) -> Result<SessionSnapshot, String> {
    let config = config::load_or_default(&app);
    Ok(state.send_text_message(&config, message).await)
}

#[tauri::command]
async fn load_runtime_config(app: tauri::AppHandle) -> Result<RuntimeConfig, String> {
    Ok(config::load_or_default(&app))
}

#[tauri::command]
async fn save_runtime_config(
    app: tauri::AppHandle,
    state: tauri::State<'_, RuntimeState>,
    config: RuntimeConfig,
) -> Result<SessionSnapshot, String> {
    config::save(&app, &config)?;
    let snapshot = state.save_config_snapshot(&config).await;
    broadcast_snapshot(&app, &snapshot);
    let _ = app.emit("runtime://config", config.clone());
    Ok(snapshot)
}

#[tauri::command]
async fn reconfigure_session(
    app: tauri::AppHandle,
    state: tauri::State<'_, RuntimeState>,
    config: RuntimeConfig,
) -> Result<SessionSnapshot, String> {
    config::save(&app, &config)?;
    let _ = state.disconnect().await;
    let _ = app.emit("runtime://config", config.clone());
    let snapshot = state.connect(config).await;
    broadcast_snapshot(&app, &snapshot);
    Ok(snapshot)
}

#[tauri::command]
async fn sync_at_state(
    app: tauri::AppHandle,
    state: tauri::State<'_, RuntimeState>,
) -> Result<SessionSnapshot, String> {
    let config = config::load_or_default(&app);
    let lines = state.at_state_lines().await;
    state.udp_send_at_state(&config, &lines).await?;
    state
        .push_runtime_event("AT 状态同步", "本地 AT 状态已下发到远端节点", "accent")
        .await;
    Ok(state.snapshot().await)
}

#[tauri::command]
async fn get_serial_tunnel_status(
    state: tauri::State<'_, RuntimeState>,
) -> Result<SerialTunnelSnapshot, String> {
    Ok(state.serial_tunnel_snapshot().await)
}

#[tauri::command]
async fn start_serial_tunnel(
    app: tauri::AppHandle,
    state: tauri::State<'_, RuntimeState>,
    mut config: SerialTunnelConfig,
) -> Result<SerialTunnelSnapshot, String> {
    config.mode = "physical".into();
    let mut runtime_config = config::load_or_default(&app);
    runtime_config.serial_tunnel = config.clone();
    config::save(&app, &runtime_config)?;
    let snapshot = state.start_serial_tunnel(config).await?;
    let _ = app.emit("runtime://serial-tunnel", snapshot.clone());
    Ok(snapshot)
}

#[tauri::command]
async fn stop_serial_tunnel(
    app: tauri::AppHandle,
    state: tauri::State<'_, RuntimeState>,
) -> Result<SerialTunnelSnapshot, String> {
    let snapshot = state.stop_serial_tunnel().await;
    let _ = app.emit("runtime://serial-tunnel", snapshot.clone());
    Ok(snapshot)
}

#[tauri::command]
async fn list_serial_ports() -> Result<Vec<String>, String> {
    Ok(serial_tunnel::available_port_names())
}

#[tauri::command]
async fn fetch_platform_servers() -> Result<Vec<PlatformServer>, String> {
    platform::fetch_platform_servers().await
}

#[tauri::command]
async fn platform_login(
    server: PlatformServer,
    username: String,
    password: String,
) -> Result<LoginBootstrap, String> {
    platform::login(server, username, password).await
}

#[tauri::command]
async fn platform_restore_session(
    api_base: String,
    token: String,
    server: PlatformServer,
    current_group_id: i32,
) -> Result<LoginBootstrap, String> {
    platform::restore_session(api_base, token, server, current_group_id).await
}

#[tauri::command]
async fn platform_login_with_token(
    server: PlatformServer,
    token: String,
) -> Result<LoginBootstrap, String> {
    platform::login_with_token(server, token).await
}

#[tauri::command]
async fn platform_fetch_device_group(
    server: String,
    callsign: String,
    ssid: u8,
) -> Result<i32, String> {
    platform::fetch_current_device_group(server, callsign, ssid).await
}

#[tauri::command]
async fn platform_register(
    host: String,
    payload: PlatformRegisterPayload,
    license_filename: String,
    license_bytes: Vec<u8>,
) -> Result<PlatformRegisterResult, String> {
    platform::register(host, payload, license_filename, license_bytes).await
}

#[tauri::command]
async fn platform_fetch_groups(
    api_base: String,
    token: String,
    current_group_id: i32,
) -> Result<GroupSnapshot, String> {
    platform::fetch_groups(api_base, token, current_group_id).await
}

#[tauri::command]
async fn platform_fetch_group_devices(
    api_base: String,
    token: String,
    group_id: i32,
) -> Result<Vec<PlatformDevice>, String> {
    platform::fetch_group_devices(api_base, token, group_id).await
}

#[tauri::command]
async fn platform_switch_group(
    api_base: String,
    token: String,
    callsign: String,
    ssid: u8,
    group_id: i32,
) -> Result<GroupSnapshot, String> {
    platform::switch_group(api_base, token, callsign, ssid, group_id).await
}

#[tauri::command]
async fn open_monitor_window(app: tauri::AppHandle) -> Result<bool, String> {
    const LABEL: &str = "room-monitor";
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.set_focus();
        return Ok(false);
    }

    WebviewWindowBuilder::new(&app, LABEL, WebviewUrl::App("index.html#monitor".into()))
        .title("房间监听")
        .inner_size(1040.0, 720.0)
        .min_inner_size(720.0, 500.0)
        .resizable(true)
        .build()
        .map_err(|err| format!("open monitor window failed: {err}"))?;

    Ok(true)
}

#[tauri::command]
async fn toggle_ptt_window(app: tauri::AppHandle) -> Result<bool, String> {
    const LABEL: &str = "ptt-float";
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.close();
        return Ok(false);
    }

    open_ptt_window(app).await
}

#[tauri::command]
async fn open_ptt_window(app: tauri::AppHandle) -> Result<bool, String> {
    const LABEL: &str = "ptt-float";
    if app.get_webview_window(LABEL).is_some() {
        return Ok(true);
    }

    WebviewWindowBuilder::new(&app, LABEL, WebviewUrl::App("index.html#ptt".into()))
        .title("PTT")
        // 双 PTT（NRL + FMO）并排 + 呼号显示行，区域加大
        .inner_size(440.0, 214.0)
        .min_inner_size(400.0, 190.0)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .build()
        .map_err(|err| format!("open ptt window failed: {err}"))?;

    Ok(true)
}

#[tauri::command]
async fn start_ptt_window_drag(app: tauri::AppHandle) -> Result<(), String> {
    const LABEL: &str = "ptt-float";
    let window = app
        .get_webview_window(LABEL)
        .ok_or_else(|| "ptt window not found".to_string())?;
    window
        .start_dragging()
        .map_err(|err| format!("start ptt drag failed: {err}"))
}

#[tauri::command]
async fn close_ptt_window(app: tauri::AppHandle) -> Result<(), String> {
    const LABEL: &str = "ptt-float";
    if let Some(window) = app.get_webview_window(LABEL) {
        window
            .destroy()
            .map_err(|err| format!("close ptt window failed: {err}"))?;
    }
    Ok(())
}

#[tauri::command]
fn get_default_audio_dir() -> String {
    dirs::audio_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string())
}

#[tauri::command]
async fn read_voice_file(file_path: String) -> Result<Vec<u8>, String> {
    tokio::task::spawn_blocking(move || std::fs::read(&file_path))
        .await
        .map_err(|err| format!("read voice file task failed: {err}"))?
        .map_err(|err| format!("read voice file failed: {err}"))
}

// ---------------------------------------------------------------- FMO commands

#[tauri::command]
async fn fmo_state_snapshot(
    state: tauri::State<'_, RuntimeState>,
) -> Result<serde_json::Value, String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    let mut out = serde_json::json!({
        "identity": {
            "callsign": fmo.current_callsign(),
            "uid": fmo.current_uid(),
        },
        "certCallsign": fmo.current_callsign(),
        "passcode": fmo::fmo_auth::aprs_passcode(&fmo.current_callsign()),
        "certs": fmo.cert_store.list().await,
        "favorites": fmo.favorites_list().await,
        "servers": fmo.server_table.to_list().await,
        "clients": fmo.server_table.client_list().await,
        "mqttState": fmo.mqtt_client.state_str().await,
        "mqttDetail": fmo.mqtt_client.detail.lock().await.clone(),
        "mqttClientId": fmo.mqtt_client.client_id_str().await,
        "aprsState": fmo.aprs_client.state.lock().await.clone(),
        "aprsDetail": fmo.aprs_client.detail.lock().await.clone(),
        "selectedServer": fmo.selected_server.lock().await.clone(),
        "rxPlay": *fmo.rx_play_enabled.lock().unwrap(),
        "mqttNoLocal": fmo.mqtt_client.no_local_enabled(),
    });
    out["mqttDetail"] = serde_json::Value::String(fmo.mqtt_client.detail.lock().await.clone());
    out["aprsDetail"] = serde_json::Value::String(fmo.aprs_client.detail.lock().await.clone());
    Ok(out)
}

/// 导入证书后的身份一致性检查：cert_user + cert_devicekey 都就位才校验，
/// 结果挂在返回条目的 identity_check 字段供前端即时提示。
fn attach_identity_check(fmo: &crate::fmo::state::FmoState, entry: &mut serde_json::Value) {
    let certs_dir = fmo.data_dir.join("certs");
    let ready = certs_dir.join("cert_user.json").is_file()
        && certs_dir.join("cert_devicekey.json").is_file();
    entry["identity_check"] = if !ready {
        serde_json::json!({"checked": false})
    } else {
        match fmo::fmo_auth::validate_identity(&certs_dir) {
            Ok(()) => serde_json::json!({"checked": true, "ok": true}),
            Err(e) => serde_json::json!({"checked": true, "ok": false, "msg": e}),
        }
    };
}

#[tauri::command]
async fn fmo_cert_import_json(
    state: tauri::State<'_, RuntimeState>,
    name: String,
    cert: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    let mut entry = fmo.cert_store.import_json(&name, cert, "json").await;
    attach_identity_check(&fmo, &mut entry);
    Ok(entry)
}

#[tauri::command]
async fn fmo_cert_import_file(
    state: tauri::State<'_, RuntimeState>,
    file_path: String,
    name: Option<String>,
) -> Result<serde_json::Value, String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    let stem = std::path::Path::new(&file_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("imported")
        .to_string();
    let raw = tokio::task::spawn_blocking(move || std::fs::read(&file_path))
        .await
        .map_err(|err| format!("read cert file task failed: {err}"))?
        .map_err(|err| format!("read cert file failed: {err}"))?;
    let obj: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|err| format!("JSON 解析失败: {err}"))?;
    let name = name.unwrap_or_else(|| {
        if ["cert_user", "cert_devicekey", "cert_int", "cert_root"].contains(&stem.as_str()) {
            stem
        } else {
            "imported".into()
        }
    });
    let mut entry = fmo.cert_store.import_json(&name, obj, "upload").await;
    attach_identity_check(&fmo, &mut entry);
    Ok(entry)
}

#[tauri::command]
async fn fmo_aprs_connect(
    state: tauri::State<'_, RuntimeState>,
    callsign: String,
    passcode: String,
) -> Result<(), String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    let params = fmo::aprs::AprsParams {
        host: fmo::aprs::DEFAULT_HOST.to_string(),
        port: fmo::aprs::DEFAULT_PORT,
        callsign,
        passcode,
        lat: 39.9,
        lon: 116.4,
        dist: 500.0,
    };
    fmo.aprs_client.connect_to(params).await;
    Ok(())
}

#[tauri::command]
async fn fmo_aprs_disconnect(state: tauri::State<'_, RuntimeState>) -> Result<(), String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    fmo.aprs_client.disconnect().await;
    Ok(())
}

#[tauri::command]
async fn fmo_server_select(
    state: tauri::State<'_, RuntimeState>,
    server: serde_json::Value,
) -> Result<(), String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    fmo.select_server(server).await;
    Ok(())
}

#[tauri::command]
async fn fmo_mqtt_connect(
    state: tauri::State<'_, RuntimeState>,
    tls: Option<bool>,
) -> Result<(), String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    // 确保音频引擎已启动（FMO 独立连接，不依赖 config.protocol）
    state.ensure_audio_running().await;
    fmo.connect_mqtt(tls.unwrap_or(false)).await
}

#[tauri::command]
async fn fmo_mqtt_disconnect(state: tauri::State<'_, RuntimeState>) -> Result<(), String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    fmo.disconnect_mqtt().await;
    Ok(())
}

#[tauri::command]
async fn fmo_favorites_add(
    state: tauri::State<'_, RuntimeState>,
    body: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    Ok(fmo.favorites_add(body).await)
}

#[tauri::command]
async fn fmo_favorites_remove(
    state: tauri::State<'_, RuntimeState>,
    key: String,
) -> Result<(), String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    fmo.favorites_remove(&key).await;
    Ok(())
}

#[tauri::command]
async fn fmo_rx_play(state: tauri::State<'_, RuntimeState>, enabled: bool) -> Result<(), String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    *fmo.rx_play_enabled.lock().unwrap() = enabled;
    Ok(())
}

#[tauri::command]
async fn fmo_mqtt_no_local(state: tauri::State<'_, RuntimeState>, enabled: bool) -> Result<(), String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    fmo.mqtt_client.set_no_local(enabled).await
}

#[tauri::command]
async fn fmo_stats_snapshot(
    state: tauri::State<'_, RuntimeState>,
) -> Result<serde_json::Value, String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    Ok(fmo.stats_snapshot().await)
}

// ---------------------------------------------------------------- FMO 设备激活（绑定 MAC 自动获取证书）

/// 激活配置：证书服务器地址 + 本机 MAC（平台上登记绑定的就是这个地址）。
#[tauri::command]
async fn fmo_activate_get_config(
    state: tauri::State<'_, RuntimeState>,
) -> Result<serde_json::Value, String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    Ok(fmo::activate::config(&fmo.data_dir))
}

#[tauri::command]
async fn fmo_activate_set_config(
    state: tauri::State<'_, RuntimeState>,
    server: String,
) -> Result<(), String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    fmo::activate::set_server(&fmo.data_dir, &server)
}

/// 执行激活：POST /api/device/activate → 写证书 → 校验身份 → MQTT 重连。
#[tauri::command]
async fn fmo_activate_run(state: tauri::State<'_, RuntimeState>) -> Result<String, String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    fmo::activate::run(&fmo).await
}

// ---------------------------------------------------------------- FMO QSO / 服务器广播

#[tauri::command]
async fn fmo_qso_call(
    state: tauri::State<'_, RuntimeState>,
    target: String,
    uid: Option<u32>,
) -> Result<(), String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    fmo.qso.call(&target, uid).await
}

#[tauri::command]
async fn fmo_qso_answer(state: tauri::State<'_, RuntimeState>, accept: bool) -> Result<(), String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    fmo.qso.answer(accept).await
}

#[tauri::command]
async fn fmo_qso_cancel(state: tauri::State<'_, RuntimeState>) -> Result<(), String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    fmo.qso.cancel().await
}

#[tauri::command]
async fn fmo_qso_state(state: tauri::State<'_, RuntimeState>) -> Result<serde_json::Value, String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    Ok(fmo.qso.snapshot().await)
}

#[tauri::command]
async fn fmo_qso_log(
    state: tauri::State<'_, RuntimeState>,
) -> Result<Vec<serde_json::Value>, String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    Ok(fmo.qso.qso_log())
}

#[tauri::command]
async fn fmo_qso_set_auto_accept(
    state: tauri::State<'_, RuntimeState>,
    enabled: bool,
) -> Result<(), String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    fmo.qso.set_auto_accept(enabled);
    Ok(())
}

#[tauri::command]
async fn fmo_broadcast_config(
    state: tauri::State<'_, RuntimeState>,
) -> Result<serde_json::Value, String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    serde_json::to_value(fmo.broadcast.config().await).map_err(|e| e.to_string())
}

#[tauri::command]
async fn fmo_broadcast_set_config(
    state: tauri::State<'_, RuntimeState>,
    config: serde_json::Value,
) -> Result<(), String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    let cfg: fmo::broadcast::BroadcastConfig =
        serde_json::from_value(config).map_err(|e| format!("广播配置格式错误: {e}"))?;
    fmo.broadcast.set_config(cfg).await
}

#[tauri::command]
async fn fmo_broadcast_now(state: tauri::State<'_, RuntimeState>) -> Result<(), String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    fmo.broadcast.send(true).await
}

/// 广播 super 门控查询：{eligible, reason, role}，供前端禁用广播开关并提示原因。
#[tauri::command]
async fn fmo_broadcast_eligible(
    state: tauri::State<'_, RuntimeState>,
) -> Result<serde_json::Value, String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    let (eligible, reason, role) = fmo.broadcast.eligible().await;
    Ok(serde_json::json!({"eligible": eligible, "reason": reason, "role": role}))
}

#[tauri::command]
async fn fmo_beacon_config(
    state: tauri::State<'_, RuntimeState>,
) -> Result<serde_json::Value, String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    serde_json::to_value(fmo.beacon.config().await).map_err(|e| e.to_string())
}

#[tauri::command]
async fn fmo_beacon_set_config(
    state: tauri::State<'_, RuntimeState>,
    config: serde_json::Value,
) -> Result<(), String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    let cfg: fmo::broadcast::BeaconConfig =
        serde_json::from_value(config).map_err(|e| format!("信标配置格式错误: {e}"))?;
    fmo.beacon.set_config(cfg).await
}

#[tauri::command]
async fn fmo_beacon_now(state: tauri::State<'_, RuntimeState>) -> Result<(), String> {
    let Some(fmo) = state.fmo_state().await else {
        return Err("FMO 未初始化".into());
    };
    fmo.beacon.send(true).await
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            g711::warmup_tables();
            runtime::manage(app);

            let main_window = app.get_webview_window("main").unwrap();
            let app_handle = app.handle().clone();
            main_window.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { .. } = event {
                    if let Some(ptt_window) = app_handle.get_webview_window("ptt-float") {
                        let _ = ptt_window.close();
                    }
                    std::process::exit(0);
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            frontend_log,
            bootstrap_runtime,
            connect_session,
            disconnect_session,
            toggle_transmit,
            set_transmit,
            set_transmit_proto,
            toggle_monitor,
            cycle_bridge_mode,
            update_jitter_buffer,
            send_text_message,
            load_runtime_config,
            save_runtime_config,
            reconfigure_session,
            sync_at_state,
            get_serial_tunnel_status,
            start_serial_tunnel,
            stop_serial_tunnel,
            list_serial_ports,
            fetch_platform_servers,
            platform_login,
            platform_login_with_token,
            platform_register,
            platform_restore_session,
            platform_fetch_groups,
            platform_fetch_group_devices,
            platform_fetch_device_group,
            platform_switch_group,
            open_monitor_window,
            open_ptt_window,
            toggle_ptt_window,
            start_ptt_window_drag,
            close_ptt_window,
            get_default_audio_dir,
            read_voice_file,
            fmo_state_snapshot,
            fmo_cert_import_json,
            fmo_cert_import_file,
            fmo_aprs_connect,
            fmo_aprs_disconnect,
            fmo_server_select,
            fmo_mqtt_connect,
            fmo_mqtt_disconnect,
            fmo_favorites_add,
            fmo_favorites_remove,
            fmo_rx_play,
            fmo_mqtt_no_local,
            fmo_stats_snapshot,
            fmo_activate_get_config,
            fmo_activate_set_config,
            fmo_activate_run,
            fmo_qso_call,
            fmo_qso_answer,
            fmo_qso_cancel,
            fmo_qso_state,
            fmo_qso_log,
            fmo_qso_set_auto_accept,
            fmo_broadcast_config,
            fmo_broadcast_set_config,
            fmo_broadcast_now,
            fmo_broadcast_eligible,
            fmo_beacon_config,
            fmo_beacon_set_config,
            fmo_beacon_now
        ])
        .run(tauri::generate_context!())
        .expect("failed to run NRL Pulse");
}
