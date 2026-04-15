mod at;
mod audio;
#[cfg(target_os = "macos")]
mod audio_aec_mac;
#[cfg(target_os = "windows")]
mod audio_aec_win;
mod config;
mod g711;
mod models;
mod nrl;
mod platform;
mod runtime;
mod udp;

use config::RuntimeConfig;
use models::{RuntimeBootstrap, SessionSnapshot};
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
    state: tauri::State<'_, RuntimeState>,
) -> Result<RuntimeBootstrap, String> {
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
async fn toggle_monitor(
    app: tauri::AppHandle,
    state: tauri::State<'_, RuntimeState>,
) -> Result<SessionSnapshot, String> {
    let snapshot = state.toggle_monitor().await;
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
        .title("NRL PTT")
        .inner_size(340.0, 150.0)
        .min_inner_size(300.0, 140.0)
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
            toggle_monitor,
            update_jitter_buffer,
            send_text_message,
            load_runtime_config,
            save_runtime_config,
            reconfigure_session,
            sync_at_state,
            fetch_platform_servers,
            platform_login,
            platform_register,
            platform_restore_session,
            platform_fetch_groups,
            platform_fetch_group_devices,
            platform_switch_group,
            open_ptt_window,
            toggle_ptt_window,
            start_ptt_window_drag,
            close_ptt_window,
            get_default_audio_dir,
            read_voice_file
        ])
        .run(tauri::generate_context!())
        .expect("failed to run NRL Pulse");
}
