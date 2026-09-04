import { invoke } from "@tauri-apps/api/core";

export function flog(...args: unknown[]): void {
  const msg = args.map(a => (typeof a === "object" ? JSON.stringify(a) : String(a))).join(" ");
  console.log(msg);
  invoke("frontend_log", { msg }).catch(() => {});
}
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  ChatMessageEvent,
  FmoBeaconConfig,
  FmoBroadcastConfig,
  FmoEvent,
  FmoQsoRecord,
  FmoQsoState,
  FmoStateSnapshot,
  FmoStatsSnapshot,
  PresenceItem,
  RealtimeAudioState,
  RuntimeConfig,
  SerialTunnelConfig,
  SerialTunnelSnapshot,
  SessionSnapshot,
  TimelineEvent,
} from "@/types";

export interface RuntimeBootstrap {
  snapshot: SessionSnapshot;
  presence: PresenceItem[];
  timeline: TimelineEvent[];
  serialTunnel: SerialTunnelSnapshot;
}

export async function bootstrapRuntime(): Promise<RuntimeBootstrap> {
  return invoke<RuntimeBootstrap>("bootstrap_runtime");
}

export async function connectSession(): Promise<SessionSnapshot> {
  return invoke<SessionSnapshot>("connect_session");
}

export async function disconnectSession(): Promise<SessionSnapshot> {
  return invoke<SessionSnapshot>("disconnect_session");
}

export async function toggleTransmit(): Promise<SessionSnapshot> {
  return invoke<SessionSnapshot>("toggle_transmit");
}

export async function setTransmit(enabled: boolean): Promise<SessionSnapshot> {
  return invoke<SessionSnapshot>("set_transmit", { enabled });
}

export async function setTransmitProto(protocol: "nrl" | "fmo", enabled: boolean): Promise<SessionSnapshot> {
  return invoke<SessionSnapshot>("set_transmit_proto", { protocol, enabled });
}

export async function toggleMonitor(): Promise<SessionSnapshot> {
  return invoke<SessionSnapshot>("toggle_monitor");
}

export async function cycleBridgeMode(): Promise<SessionSnapshot> {
  return invoke<SessionSnapshot>("cycle_bridge_mode");
}

export async function updateJitterBuffer(value: number): Promise<SessionSnapshot> {
  return invoke<SessionSnapshot>("update_jitter_buffer", { value });
}

export async function sendTextMessage(message: string): Promise<SessionSnapshot> {
  return invoke<SessionSnapshot>("send_text_message", { message });
}

export async function loadRuntimeConfig(): Promise<RuntimeConfig> {
  return invoke<RuntimeConfig>("load_runtime_config");
}

export async function saveRuntimeConfig(config: RuntimeConfig): Promise<SessionSnapshot> {
  return invoke<SessionSnapshot>("save_runtime_config", { config });
}

export async function reconfigureSession(config: RuntimeConfig): Promise<SessionSnapshot> {
  return invoke<SessionSnapshot>("reconfigure_session", { config });
}

export async function syncAtState(): Promise<SessionSnapshot> {
  return invoke<SessionSnapshot>("sync_at_state");
}

export async function getSerialTunnelStatus(): Promise<SerialTunnelSnapshot> {
  return invoke<SerialTunnelSnapshot>("get_serial_tunnel_status");
}

export async function startSerialTunnel(config: SerialTunnelConfig): Promise<SerialTunnelSnapshot> {
  return invoke<SerialTunnelSnapshot>("start_serial_tunnel", { config });
}

export async function stopSerialTunnel(): Promise<SerialTunnelSnapshot> {
  return invoke<SerialTunnelSnapshot>("stop_serial_tunnel");
}

export async function listSerialPorts(): Promise<string[]> {
  return invoke<string[]>("list_serial_ports");
}

export async function getDefaultAudioDir(): Promise<string> {
  return invoke<string>("get_default_audio_dir");
}

export async function readVoiceFile(filePath: string): Promise<number[]> {
  return invoke<number[]>("read_voice_file", { filePath });
}

export async function togglePttWindow(): Promise<boolean> {
  return invoke<boolean>("toggle_ptt_window");
}

export async function openMonitorWindow(): Promise<boolean> {
  return invoke<boolean>("open_monitor_window");
}

export async function openPttWindow(): Promise<boolean> {
  return invoke<boolean>("open_ptt_window");
}

export async function startPttWindowDrag(): Promise<void> {
  return invoke<void>("start_ptt_window_drag");
}

export async function closePttWindow(): Promise<void> {
  return invoke<void>("close_ptt_window");
}

export async function onRuntimeSnapshot(
  handler: (snapshot: SessionSnapshot) => void,
): Promise<UnlistenFn> {
  return listen<SessionSnapshot>("runtime://snapshot", (event) => handler(event.payload));
}

export async function onRealtimeAudioState(
  handler: (audioState: RealtimeAudioState) => void,
): Promise<UnlistenFn> {
  return listen<RealtimeAudioState>("runtime://audio-state", (event) => handler(event.payload));
}

export async function onRuntimeConfig(
  handler: (config: RuntimeConfig) => void,
): Promise<UnlistenFn> {
  return listen<RuntimeConfig>("runtime://config", (event) => handler(event.payload));
}

export async function onSerialTunnel(
  handler: (snapshot: SerialTunnelSnapshot) => void,
): Promise<UnlistenFn> {
  return listen<SerialTunnelSnapshot>("runtime://serial-tunnel", (event) => handler(event.payload));
}

export async function onPresence(
  handler: (presence: PresenceItem[]) => void,
): Promise<UnlistenFn> {
  return listen<PresenceItem[]>("runtime://presence", (event) => handler(event.payload));
}

export async function onTimeline(
  handler: (event: TimelineEvent) => void,
): Promise<UnlistenFn> {
  return listen<TimelineEvent>("runtime://timeline", (event) => handler(event.payload));
}

export async function onChatMessage(
  handler: (event: ChatMessageEvent) => void,
): Promise<UnlistenFn> {
  return listen<ChatMessageEvent>("runtime://chat-message", (event) => handler(event.payload));
}

// ---------------------------------------------------------------- FMO

export async function fmoStateSnapshot(): Promise<FmoStateSnapshot> {
  return invoke<FmoStateSnapshot>("fmo_state_snapshot");
}

export async function fmoStatsSnapshot(): Promise<FmoStatsSnapshot> {
  return invoke<FmoStatsSnapshot>("fmo_stats_snapshot");
}

export async function fmoCertImportJson(
  name: string,
  cert: Record<string, unknown>,
): Promise<unknown> {
  return invoke("fmo_cert_import_json", { name, cert });
}

export async function fmoCertImportFile(
  filePath: string,
  name?: string,
): Promise<unknown> {
  return invoke("fmo_cert_import_file", { filePath, name });
}

export async function fmoAprsConnect(callsign: string, passcode: string): Promise<void> {
  return invoke("fmo_aprs_connect", { callsign, passcode });
}

export async function fmoAprsDisconnect(): Promise<void> {
  return invoke("fmo_aprs_disconnect");
}

export async function fmoAprsSetServer(host: string): Promise<void> {
  return invoke("fmo_aprs_set_server", { host });
}

export async function fmoServerSelect(server: Record<string, unknown>): Promise<void> {
  return invoke("fmo_server_select", { server });
}

export async function fmoMqttConnect(tls?: boolean): Promise<void> {
  return invoke("fmo_mqtt_connect", { tls: tls ?? false });
}

export async function fmoMqttDisconnect(): Promise<void> {
  return invoke("fmo_mqtt_disconnect");
}

export async function fmoFavoritesAdd(body: Record<string, unknown>): Promise<unknown> {
  return invoke("fmo_favorites_add", { body });
}

export async function fmoFavoritesRemove(key: string): Promise<void> {
  return invoke("fmo_favorites_remove", { key });
}

export async function fmoRxPlay(enabled: boolean): Promise<void> {
  return invoke("fmo_rx_play", { enabled });
}

export async function fmoMqttNoLocal(enabled: boolean): Promise<void> {
  return invoke("fmo_mqtt_no_local", { enabled });
}

// FMO 设备激活（绑定 MAC 自动获取证书）：server 为证书服务器地址，mac 为本机 MAC
export interface FmoActivateConfig {
  server: string;
  mac: string;
}

export async function fmoActivateGetConfig(): Promise<FmoActivateConfig> {
  return invoke<FmoActivateConfig>("fmo_activate_get_config");
}

export async function fmoActivateSetConfig(server: string): Promise<void> {
  return invoke("fmo_activate_set_config", { server });
}

export async function fmoActivateRun(): Promise<string> {
  return invoke<string>("fmo_activate_run");
}

export async function fmoQsoCall(target: string, uid?: number): Promise<void> {
  return invoke("fmo_qso_call", { target, uid: uid ?? null });
}

export async function fmoQsoAnswer(accept: boolean): Promise<void> {
  return invoke("fmo_qso_answer", { accept });
}

export async function fmoQsoCancel(): Promise<void> {
  return invoke("fmo_qso_cancel");
}

export async function fmoQsoState(): Promise<FmoQsoState> {
  return invoke<FmoQsoState>("fmo_qso_state");
}

export async function fmoQsoLog(): Promise<FmoQsoRecord[]> {
  return invoke<FmoQsoRecord[]>("fmo_qso_log");
}

export async function fmoQsoSetAutoAccept(enabled: boolean): Promise<void> {
  return invoke("fmo_qso_set_auto_accept", { enabled });
}

export async function fmoBroadcastConfig(): Promise<FmoBroadcastConfig> {
  return invoke<FmoBroadcastConfig>("fmo_broadcast_config");
}

export async function fmoBroadcastSetConfig(cfg: FmoBroadcastConfig): Promise<void> {
  return invoke("fmo_broadcast_set_config", { config: cfg });
}

export async function fmoBroadcastNow(): Promise<void> {
  return invoke("fmo_broadcast_now");
}

// 直连自己的服务器（发布场景）：用广播配置里的地址 + 本机证书身份（super）连接，
// 无需先从服务器列表选择
export async function fmoBroadcastConnect(host: string, port: number): Promise<void> {
  return invoke("fmo_broadcast_connect", { host, port });
}

export async function fmoBeaconConfig(): Promise<FmoBeaconConfig> {
  return invoke<FmoBeaconConfig>("fmo_beacon_config");
}

export async function fmoBeaconSetConfig(cfg: FmoBeaconConfig): Promise<void> {
  return invoke("fmo_beacon_set_config", { config: cfg });
}

export async function fmoBeaconNow(): Promise<void> {
  return invoke("fmo_beacon_now");
}

// 广播 super 门控查询（后端权威判定）：eligible=false 时 reason 为中文原因
export interface FmoBroadcastEligibility {
  eligible: boolean;
  reason: string;
  role: string;
}

export async function fmoBroadcastEligible(): Promise<FmoBroadcastEligibility> {
  return invoke<FmoBroadcastEligibility>("fmo_broadcast_eligible");
}

export async function onFmoEvent(handler: (event: FmoEvent) => void): Promise<UnlistenFn> {
  return listen<FmoEvent>("fmo://event", (event) => handler(event.payload));
}

export interface FmoAudioState {
  rxLevel: number;
  rxSpectrum: number[];
  txLevel: number;
  txSpectrum: number[];
  jitterMs?: number;
  queuedFrames?: number;
  downlinkKbps?: number;
  rxCodec?: string;
  rxFrames?: number;
}

export async function onFmoAudioState(
  handler: (state: FmoAudioState) => void,
): Promise<UnlistenFn> {
  return listen<FmoAudioState>("fmo://audio-state", (event) => handler(event.payload));
}

export interface UpdateInfo {
  available: boolean;
  version?: string;
  body?: string;
}

export async function checkUpdate(): Promise<UpdateInfo> {
  try {
    const { check } = await import("@tauri-apps/plugin-updater");
    const update = await check();
    if (update?.available) {
      return { available: true, version: update.version, body: update.body ?? "" };
    }
    return { available: false };
  } catch {
    return { available: false };
  }
}

export async function downloadAndInstallUpdate(
  onProgress: (downloaded: number, total: number | null) => void,
): Promise<void> {
  const { check } = await import("@tauri-apps/plugin-updater");
  const { relaunch } = await import("@tauri-apps/plugin-process");
  const update = await check();
  if (!update?.available) return;
  let downloaded = 0;
  await update.downloadAndInstall((event) => {
    if (event.event === "Progress") {
      downloaded += event.data.chunkLength;
      onProgress(downloaded, event.data.contentLength ?? null);
    }
  });
  // 安装器完成替换后由 Tauri 重启应用。直接 exit 只会退出当前进程，
  // 不会启动安装后的新版本，也容易让用户继续从下载缓存启动裸 exe。
  await relaunch();
}
