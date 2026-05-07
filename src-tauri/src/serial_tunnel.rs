use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, RwLock};

use crate::config::SerialTunnelConfig;
use crate::models::SerialTunnelSnapshot;

#[derive(Clone)]
pub struct SerialTunnel {
    running: Arc<AtomicBool>,
    write_tx: Arc<RwLock<Option<mpsc::UnboundedSender<Vec<u8>>>>>,
    snapshot: Arc<RwLock<SerialTunnelSnapshot>>,
    rx_bytes: Arc<AtomicU64>,
    tx_bytes: Arc<AtomicU64>,
}

impl SerialTunnel {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            write_tx: Arc::new(RwLock::new(None)),
            snapshot: Arc::new(RwLock::new(SerialTunnelSnapshot {
                running: false,
                supported: true,
                mode: "physical".into(),
                port_name: default_port_name(),
                status: "stopped".into(),
                rx_bytes: 0,
                tx_bytes: 0,
                last_error: String::new(),
            })),
            rx_bytes: Arc::new(AtomicU64::new(0)),
            tx_bytes: Arc::new(AtomicU64::new(0)),
        }
    }

    pub async fn snapshot(&self) -> SerialTunnelSnapshot {
        let mut snapshot = self.snapshot.read().await.clone();
        snapshot.rx_bytes = self.rx_bytes.load(Ordering::Relaxed);
        snapshot.tx_bytes = self.tx_bytes.load(Ordering::Relaxed);
        snapshot.running = self.running.load(Ordering::Relaxed);
        snapshot
    }

    pub async fn start(
        &self,
        config: SerialTunnelConfig,
        read_tx: mpsc::UnboundedSender<Vec<u8>>,
    ) -> Result<SerialTunnelSnapshot, String> {
        self.stop().await;

        if config.mode == "physical" {
            validate_physical_serial(&config)?;
        }

        if config.mode == "virtual" && !cfg!(target_os = "windows") {
            let mut snapshot = self.snapshot.write().await;
            snapshot.supported = false;
            snapshot.running = false;
            snapshot.mode = config.mode;
            snapshot.port_name = config.port_name;
            snapshot.status = "unsupported".into();
            snapshot.last_error = "当前系统暂不支持注册虚拟 COM 串口".into();
            return Err(snapshot.last_error.clone());
        }

        self.rx_bytes.store(0, Ordering::Relaxed);
        self.tx_bytes.store(0, Ordering::Relaxed);
        self.running.store(true, Ordering::Relaxed);
        let (write_tx, write_rx) = mpsc::unbounded_channel();
        {
            let mut guard = self.write_tx.write().await;
            *guard = Some(write_tx);
        }
        {
            let mut snapshot = self.snapshot.write().await;
            snapshot.supported = config.mode == "physical" || cfg!(target_os = "windows");
            snapshot.running = true;
            snapshot.mode = config.mode.clone();
            snapshot.port_name = config.port_name.clone();
            snapshot.status = "starting".into();
            snapshot.last_error.clear();
            snapshot.rx_bytes = 0;
            snapshot.tx_bytes = 0;
        }

        self.spawn_worker(config, read_tx, write_rx);
        Ok(self.snapshot().await)
    }

    pub async fn stop(&self) -> SerialTunnelSnapshot {
        self.running.store(false, Ordering::Relaxed);
        {
            let mut guard = self.write_tx.write().await;
            *guard = None;
        }
        {
            let mut snapshot = self.snapshot.write().await;
            snapshot.running = false;
            if snapshot.status != "unsupported" {
                snapshot.status = "stopped".into();
            }
        }
        self.snapshot().await
    }

    pub async fn write(&self, data: &[u8]) -> Result<(), String> {
        let tx = self.write_tx.read().await.clone();
        let Some(tx) = tx else {
            return Err("串口透传未启动".into());
        };
        let size = data.len();
        tx.send(data.to_vec())
            .map_err(|_| "串口透传写入通道已关闭".to_string())?;
        self.tx_bytes.fetch_add(size as u64, Ordering::Relaxed);
        Ok(())
    }

    fn spawn_worker(
        &self,
        config: SerialTunnelConfig,
        read_tx: mpsc::UnboundedSender<Vec<u8>>,
        write_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    ) {
        let running = self.running.clone();
        let snapshot = self.snapshot.clone();
        let rx_bytes = self.rx_bytes.clone();
        let tx_bytes = self.tx_bytes.clone();
        tauri::async_runtime::spawn_blocking(move || {
            if config.mode == "virtual" {
                platform::run_virtual_serial(
                    config, running, snapshot, rx_bytes, tx_bytes, read_tx, write_rx,
                );
            } else {
                run_physical_serial(
                    config, running, snapshot, rx_bytes, tx_bytes, read_tx, write_rx,
                );
            }
        });
    }
}

pub fn available_port_names() -> Vec<String> {
    serialport::available_ports()
        .map(|ports| ports.into_iter().map(|port| port.port_name).collect())
        .unwrap_or_default()
}

fn run_physical_serial(
    config: SerialTunnelConfig,
    running: Arc<AtomicBool>,
    snapshot: Arc<RwLock<SerialTunnelSnapshot>>,
    rx_bytes: Arc<AtomicU64>,
    _tx_bytes: Arc<AtomicU64>,
    read_tx: mpsc::UnboundedSender<Vec<u8>>,
    mut write_rx: mpsc::UnboundedReceiver<Vec<u8>>,
) {
    let builder = serialport::new(&config.port_name, config.baud_rate)
        .timeout(Duration::from_millis(20))
        .data_bits(to_data_bits(config.data_bits))
        .parity(to_parity(&config.parity))
        .stop_bits(to_stop_bits(&config.stop_bits))
        .flow_control(to_flow_control(&config.flow_control));

    let mut port = match builder.open() {
        Ok(port) => port,
        Err(err) => {
            set_status(
                &snapshot,
                false,
                "error",
                format!("打开物理串口 {} 失败: {err}", config.port_name),
            );
            running.store(false, Ordering::Relaxed);
            return;
        }
    };

    set_status(&snapshot, true, "connected", String::new());
    let mut buf = [0_u8; 1024];
    while running.load(Ordering::Relaxed) {
        while let Ok(data) = write_rx.try_recv() {
            if data.is_empty() {
                continue;
            }
            match port.write_all(&data) {
                Ok(()) => {}
                Err(err) => {
                    set_status(
                        &snapshot,
                        false,
                        "error",
                        format!("物理串口写入失败: {err}"),
                    );
                    running.store(false, Ordering::Relaxed);
                    break;
                }
            }
        }

        match port.read(&mut buf) {
            Ok(size) if size > 0 => {
                rx_bytes.fetch_add(size as u64, Ordering::Relaxed);
                let _ = read_tx.send(buf[..size].to_vec());
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::TimedOut => {}
            Err(err) => {
                set_status(
                    &snapshot,
                    false,
                    "error",
                    format!("物理串口读取失败: {err}"),
                );
                running.store(false, Ordering::Relaxed);
                break;
            }
        }
    }
    set_status(&snapshot, false, "stopped", String::new());
}

fn validate_physical_serial(config: &SerialTunnelConfig) -> Result<(), String> {
    if config.port_name.trim().is_empty() {
        return Err("请选择物理串口".into());
    }
    serialport::new(&config.port_name, config.baud_rate)
        .timeout(Duration::from_millis(20))
        .data_bits(to_data_bits(config.data_bits))
        .parity(to_parity(&config.parity))
        .stop_bits(to_stop_bits(&config.stop_bits))
        .flow_control(to_flow_control(&config.flow_control))
        .open()
        .map(|_| ())
        .map_err(|err| format!("打开物理串口 {} 失败: {err}", config.port_name))
}

fn to_data_bits(value: u8) -> serialport::DataBits {
    match value {
        5 => serialport::DataBits::Five,
        6 => serialport::DataBits::Six,
        7 => serialport::DataBits::Seven,
        _ => serialport::DataBits::Eight,
    }
}

fn to_parity(value: &str) -> serialport::Parity {
    match value {
        "odd" => serialport::Parity::Odd,
        "even" => serialport::Parity::Even,
        _ => serialport::Parity::None,
    }
}

fn to_stop_bits(value: &str) -> serialport::StopBits {
    match value {
        "two" => serialport::StopBits::Two,
        _ => serialport::StopBits::One,
    }
}

fn to_flow_control(value: &str) -> serialport::FlowControl {
    match value {
        "software" => serialport::FlowControl::Software,
        "hardware" => serialport::FlowControl::Hardware,
        _ => serialport::FlowControl::None,
    }
}

fn default_port_name() -> String {
    #[cfg(target_os = "windows")]
    {
        "COM1".into()
    }
    #[cfg(target_os = "macos")]
    {
        "/dev/tty.usbserial".into()
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        "/dev/ttyUSB0".into()
    }
    #[cfg(not(any(target_os = "windows", unix)))]
    {
        "COM1".into()
    }
}

fn set_status(
    snapshot: &Arc<RwLock<SerialTunnelSnapshot>>,
    running: bool,
    status: &str,
    last_error: String,
) {
    let snapshot = snapshot.clone();
    tauri::async_runtime::block_on(async move {
        let mut guard = snapshot.write().await;
        guard.running = running;
        guard.status = status.into();
        guard.last_error = last_error;
    });
}

#[cfg(not(target_os = "windows"))]
mod platform {
    use super::*;

    pub fn run_virtual_serial(
        _config: SerialTunnelConfig,
        _running: Arc<AtomicBool>,
        _snapshot: Arc<RwLock<SerialTunnelSnapshot>>,
        _rx_bytes: Arc<AtomicU64>,
        _tx_bytes: Arc<AtomicU64>,
        _read_tx: mpsc::UnboundedSender<Vec<u8>>,
        _write_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    ) {
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use std::ffi::OsStr;
    use std::iter::once;
    use std::os::windows::ffi::OsStrExt;
    use std::thread;
    use std::time::Duration;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING,
    };
    use windows::Win32::Storage::FileSystem::{
        DefineDosDeviceW, ReadFile, WriteFile, DDD_RAW_TARGET_PATH, DDD_REMOVE_DEFINITION,
        PIPE_ACCESS_DUPLEX,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PeekNamedPipe, PIPE_NOWAIT,
        PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    };

    pub fn run_virtual_serial(
        config: SerialTunnelConfig,
        running: Arc<AtomicBool>,
        snapshot: Arc<RwLock<SerialTunnelSnapshot>>,
        rx_bytes: Arc<AtomicU64>,
        _tx_bytes: Arc<AtomicU64>,
        read_tx: mpsc::UnboundedSender<Vec<u8>>,
        mut write_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    ) {
        let pipe_name = format!(r"\\.\pipe\nrl-pulse-{}", config.port_name);
        let device_target = format!(r"\Device\NamedPipe\nrl-pulse-{}", config.port_name);
        let pipe_name_w = wide(&pipe_name);
        let port_name_w = wide(&config.port_name);
        let target_w = wide(&device_target);

        let mapped = unsafe {
            DefineDosDeviceW(
                DDD_RAW_TARGET_PATH,
                PCWSTR(port_name_w.as_ptr()),
                PCWSTR(target_w.as_ptr()),
            )
            .is_ok()
        };
        if !mapped {
            set_status(
                &snapshot,
                false,
                "error",
                format!("注册 {} 失败: {:?}", config.port_name, unsafe {
                    GetLastError()
                }),
            );
            running.store(false, Ordering::Relaxed);
            return;
        }

        set_status(&snapshot, true, "waiting", String::new());

        while running.load(Ordering::Relaxed) {
            let handle = unsafe {
                CreateNamedPipeW(
                    PCWSTR(pipe_name_w.as_ptr()),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    4096,
                    4096,
                    0,
                    None,
                )
            };
            if handle.is_invalid() {
                set_status(
                    &snapshot,
                    false,
                    "error",
                    format!("创建虚拟串口管道失败: {:?}", unsafe {
                        GetLastError()
                    }),
                );
                running.store(false, Ordering::Relaxed);
                break;
            }

            let mut connected = false;
            while running.load(Ordering::Relaxed) {
                if unsafe { ConnectNamedPipe(handle, None).is_ok() } {
                    connected = true;
                    break;
                }
                let err = unsafe { GetLastError() };
                if err == ERROR_PIPE_CONNECTED {
                    connected = true;
                    break;
                }
                if err != ERROR_PIPE_LISTENING {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            if !connected {
                unsafe {
                    let _ = CloseHandle(handle);
                }
                thread::sleep(Duration::from_millis(100));
                continue;
            }

            set_status(&snapshot, true, "connected", String::new());
            while running.load(Ordering::Relaxed) {
                while let Ok(data) = write_rx.try_recv() {
                    if data.is_empty() {
                        continue;
                    }
                    let mut written = 0_u32;
                    let ok =
                        unsafe { WriteFile(handle, Some(&data), Some(&mut written), None).is_ok() };
                    if !ok {
                        break;
                    }
                }

                let mut available = 0_u32;
                let has_data = unsafe {
                    PeekNamedPipe(handle, None, 0, None, Some(&mut available), None).is_ok()
                };
                if !has_data || available == 0 {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }

                let mut buf = [0_u8; 1024];
                let mut read = 0_u32;
                let ok = unsafe { ReadFile(handle, Some(&mut buf), Some(&mut read), None).is_ok() };
                if ok && read > 0 {
                    let data = buf[..read as usize].to_vec();
                    rx_bytes.fetch_add(u64::from(read), Ordering::Relaxed);
                    let _ = read_tx.send(data);
                } else {
                    thread::sleep(Duration::from_millis(10));
                }
            }

            unsafe {
                let _ = DisconnectNamedPipe(handle);
                let _ = CloseHandle(handle);
            }
            if running.load(Ordering::Relaxed) {
                set_status(&snapshot, true, "waiting", String::new());
            }
        }

        unsafe {
            let _ = DefineDosDeviceW(
                DDD_REMOVE_DEFINITION | DDD_RAW_TARGET_PATH,
                PCWSTR(port_name_w.as_ptr()),
                PCWSTR(target_w.as_ptr()),
            );
        }
        set_status(&snapshot, false, "stopped", String::new());
    }

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(once(0)).collect()
    }

    fn set_status(
        snapshot: &Arc<RwLock<SerialTunnelSnapshot>>,
        running: bool,
        status: &str,
        last_error: String,
    ) {
        let snapshot = snapshot.clone();
        tauri::async_runtime::block_on(async move {
            let mut guard = snapshot.write().await;
            guard.running = running;
            guard.status = status.into();
            guard.last_error = last_error;
        });
    }
}
