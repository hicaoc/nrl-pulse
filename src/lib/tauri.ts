import { invoke } from "@tauri-apps/api/core";

export function flog(...args: unknown[]): void {
  const msg = args.map(a => (typeof a === "object" ? JSON.stringify(a) : String(a))).join(" ");
  console.log(msg);
  invoke("frontend_log", { msg }).catch(() => {});
}
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  ChatMessageEvent,
  PresenceItem,
  RuntimeConfig,
  SessionSnapshot,
  TimelineEvent,
} from "@/types";

export interface RuntimeBootstrap {
  snapshot: SessionSnapshot;
  presence: PresenceItem[];
  timeline: TimelineEvent[];
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

export async function syncAtState(): Promise<SessionSnapshot> {
  return invoke<SessionSnapshot>("sync_at_state");
}

export async function getDefaultAudioDir(): Promise<string> {
  return invoke<string>("get_default_audio_dir");
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
  await relaunch();
}
