import { invoke } from "@tauri-apps/api/core";

export function flog(...args: unknown[]): void {
  const msg = args.map(a => (typeof a === "object" ? JSON.stringify(a) : String(a))).join(" ");
  console.log(msg);
  invoke("frontend_log", { msg }).catch(() => {});
}
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  ChatMessageEvent,
  FmoEvent,
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

export async function fmoRxLoop(enabled: boolean): Promise<void> {
  return invoke("fmo_rx_loop", { enabled });
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
  const { relaunch, exit: processExit } = await import("@tauri-apps/plugin-process");
  const update = await check();
  if (!update?.available) return;
  let downloaded = 0;
  await update.downloadAndInstall((event) => {
    if (event.event === "Progress") {
      downloaded += event.data.chunkLength;
      onProgress(downloaded, event.data.contentLength ?? null);
    }
  });
  await new Promise((resolve) => setTimeout(resolve, 3000));
  await processExit(0);
}
