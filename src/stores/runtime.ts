import { computed, ref } from "vue";
import { defineStore } from "pinia";
import type { UnlistenFn } from "@tauri-apps/api/event";
import { flog } from "@/lib/tauri";
import {
  bootstrapRuntime,
  connectSession,
  disconnectSession,
  loadRuntimeConfig,
  onRealtimeAudioState,
  onPresence,
  onRuntimeConfig,
  onRuntimeSnapshot,
  onTimeline,
  reconfigureSession,
  saveRuntimeConfig,
  sendTextMessage,
  setTransmit,
  syncAtState,
  toggleMonitor,
  toggleTransmit,
  updateJitterBuffer,
} from "@/lib/tauri";
import type {
  PresenceItem,
  RealtimeAudioState,
  RuntimeConfig,
  SessionSnapshot,
  TimelineEvent,
} from "@/types";

const initialSnapshot: SessionSnapshot = {
  roomName: "NRL Command Room",
  callsign: "B1NRL",
  ssid: 7,
  activeSpeaker: "BG5XYZ",
  activeSpeakerSsid: 1,
  connection: "connecting",
  packetLoss: 0.6,
  latencyMs: 84,
  jitterMs: 22,
  uplinkKbps: 12.8,
  downlinkKbps: 13.6,
  rxLevel: 0.58,
  txLevel: 0.16,
  rxSpectrum: Array.from({ length: 28 }, () => 0),
  txSpectrum: Array.from({ length: 28 }, () => 0),
  isTransmitting: false,
  isMonitoring: true,
  queuedFrames: 4,
  lastTextMessage: "系统初始化中",
  devices: {
    inputDevice: "Default Microphone",
    outputDevice: "Default Speaker",
    sampleRate: 8000,
    jitterBufferMs: 120,
    agcEnabled: true,
    noiseSuppression: true,
    aecEnabled: false,
  },
};

export const useRuntimeStore = defineStore("runtime", () => {
  const snapshot = ref<SessionSnapshot>(initialSnapshot);
  const presence = ref<PresenceItem[]>([]);
  const timeline = ref<TimelineEvent[]>([]);
  const config = ref<RuntimeConfig>({
    server: "127.0.0.1",
    port: 10024,
    serverName: "Local",
    apiBase: "",
    authToken: "",
    loginUsername: "",
    callsign: "B1NRL",
    ssid: 7,
    roomName: "NRL East Hub",
    currentGroupId: 0,
    volume: 1,
    pttKey: "Space",
    voiceSavePath: "",
  });
  const bootstrapped = ref(false);
  const busy = ref(false);
  const unlisteners: UnlistenFn[] = [];
  let pendingSnapshot: SessionSnapshot | null = null;
  let snapshotRafId = 0;

  const connectionText = computed(() => {
    const map: Record<SessionSnapshot["connection"], string> = {
      disconnected: "离线",
      connecting: "连接中",
      connected: "已连接",
      recovering: "重连恢复中",
    };
    return map[snapshot.value.connection];
  });

  const qualityScore = computed(() => {
    const { packetLoss, latencyMs, jitterMs } = snapshot.value;
    const score = 100 - packetLoss * 18 - latencyMs * 0.12 - jitterMs * 0.35;
    return Math.max(26, Math.min(99, Math.round(score)));
  });

  function mergeSnapshot(next: SessionSnapshot) {
    snapshot.value = next;
  }

  function mergeRealtimeAudioState(next: RealtimeAudioState) {
    snapshot.value.activeSpeaker = next.activeSpeaker;
    snapshot.value.activeSpeakerSsid = next.activeSpeakerSsid;
    snapshot.value.rxLevel = next.rxLevel;
    snapshot.value.txLevel = next.txLevel;
    snapshot.value.rxSpectrum = next.rxSpectrum;
    snapshot.value.txSpectrum = next.txSpectrum;
    snapshot.value.queuedFrames = next.queuedFrames;
    snapshot.value.uplinkKbps = next.uplinkKbps;
    snapshot.value.downlinkKbps = next.downlinkKbps;
    snapshot.value.isTransmitting = next.isTransmitting;
  }

  function scheduleSnapshotFlush(next: SessionSnapshot) {
    pendingSnapshot = next;
    if (snapshotRafId) {
      return;
    }
    snapshotRafId = window.requestAnimationFrame(() => {
      snapshotRafId = 0;
      if (pendingSnapshot) {
        snapshot.value = pendingSnapshot;
        pendingSnapshot = null;
      }
    });
  }

  function pushTimeline(event: TimelineEvent) {
    timeline.value = [event, ...timeline.value].slice(0, 10);
  }

  async function bootstrap() {
    if (bootstrapped.value) {
      return;
    }
    // 先订阅事件，再拉初始数据，避免两步之间的 emit 丢失
    // 保存 unlisten 引用，防止 listener 被 GC 回收
    unlisteners.push(await onRuntimeSnapshot((next) => {
      // 高频 snapshot 只保留最新一帧，避免音频持续接收时前端事件排队，
      // 导致 UI 补播旧状态而表现为“页面卡住、显示慢于语音”。
      scheduleSnapshotFlush(next);
    }));
    unlisteners.push(await onRealtimeAudioState((next) => {
      mergeRealtimeAudioState(next);
    }));
    unlisteners.push(await onPresence((next) => {
      presence.value = next;
    }));
    unlisteners.push(await onTimeline((event) => {
      pushTimeline(event);
    }));
    unlisteners.push(await onRuntimeConfig((next) => {
      config.value = next;
    }));

    const data = await bootstrapRuntime();
    flog("[runtime] bootstrap snapshot connection=", data.snapshot.connection);
    snapshot.value = data.snapshot;
    presence.value = data.presence;
    timeline.value = data.timeline;
    config.value = await loadRuntimeConfig();
    bootstrapped.value = true;
  }

  async function runAction(action: () => Promise<SessionSnapshot>) {
    if (busy.value) {
      return;
    }
    busy.value = true;
    try {
      // 超时保护：后台在切换群组时可能因音频线程锁竞争死锁，导致 IPC 永不返回
      // 超时后强制释放 busy，避免所有按钮永久禁用
      const timeout = new Promise<never>((_, reject) =>
        setTimeout(() => reject(new Error("操作超时，请重试")), 6_000)
      );
      mergeSnapshot(await Promise.race([action(), timeout]));
    } catch (e) {
      flog("[runtime] runAction error:", String(e));
    } finally {
      busy.value = false;
    }
  }

  async function connect() {
    await runAction(connectSession);
  }

  async function disconnect() {
    await runAction(disconnectSession);
  }

  async function toggleTx() {
    await runAction(toggleTransmit);
  }

  async function setTx(enabled: boolean) {
    await runAction(() => setTransmit(enabled));
  }

  async function toggleRx() {
    await runAction(toggleMonitor);
  }

  async function setJitter(value: number) {
    await runAction(() => updateJitterBuffer(value));
  }

  async function sendMessage(message: string) {
    const text = message.trim();
    if (!text) {
      return;
    }
    await runAction(() => sendTextMessage(text));
  }

  async function saveConfig(next: RuntimeConfig) {
    config.value = next;
    await runAction(() => saveRuntimeConfig(next));
  }

  async function reconnectWithConfig(next: RuntimeConfig) {
    config.value = next;
    await runAction(() => reconfigureSession(next));
  }

  async function syncAt() {
    await runAction(syncAtState);
  }

  return {
    snapshot,
    presence,
    timeline,
    config,
    bootstrapped,
    busy,
    connectionText,
    qualityScore,
    bootstrap,
    connect,
    disconnect,
    toggleTx,
    setTx,
    toggleRx,
    setJitter,
    sendMessage,
    saveConfig,
    reconnectWithConfig,
    syncAt,
  };
});
