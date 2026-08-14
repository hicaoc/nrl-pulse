import { ref } from "vue";
import { defineStore } from "pinia";
import type { UnlistenFn } from "@tauri-apps/api/event";
import { flog } from "@/lib/tauri";
import type { FmoAudioState } from "@/lib/tauri";
import {
  fmoAprsConnect,
  fmoAprsDisconnect,
  fmoBroadcastConfig,
  fmoBroadcastNow,
  fmoBroadcastSetConfig,
  fmoCertImportFile,
  fmoCertImportJson,
  fmoFavoritesAdd,
  fmoFavoritesRemove,
  fmoMqttConnect,
  fmoMqttDisconnect,
  fmoMqttNoLocal,
  fmoQsoAnswer,
  fmoQsoCall,
  fmoQsoCancel,
  fmoQsoLog,
  fmoQsoSetAutoAccept,
  fmoQsoState,
  fmoRxPlay,
  fmoServerSelect,
  fmoStateSnapshot,
  fmoStatsSnapshot,
  onFmoAudioState,
  onFmoEvent,
} from "@/lib/tauri";
import type { FmoBroadcastConfig, FmoEvent, FmoQsoRecord, FmoQsoState, FmoServer, FmoServerTraffic, FmoStateSnapshot, FmoStatsSnapshot } from "@/types";

const initial: FmoStateSnapshot = {
  identity: { callsign: "", uid: 0 },
  certCallsign: "",
  passcode: "",
  certs: [],
  favorites: [],
  servers: [],
  clients: [],
  mqttState: "disconnected",
  mqttDetail: "",
  mqttClientId: "",
  aprsState: "disconnected",
  aprsDetail: "",
  selectedServer: null,
  rxPlay: true,
  mqttNoLocal: true,
};

const initialStats: FmoStatsSnapshot = {
  callsign: "",
  uid: 0,
  mqttState: "disconnected",
  mqttDetail: "",
  mqttClientId: "",
  aprsState: "disconnected",
  aprsDetail: "",
  serverHost: "",
  serverPort: 0,
  serverName: "",
  activeSpeaker: "",
  rxFrames: 0,
  txFrames: 0,
  rxText: 0,
  txPackets: 0,
  serverInfo: 0,
  rxLevel: 0,
  rxSpectrum: Array.from({ length: 28 }, () => 0),
  txLevel: 0,
  txSpectrum: Array.from({ length: 28 }, () => 0),
  jitterMs: 0,
  latencyMs: 0,
  packetLoss: 0,
  queuedFrames: 0,
  downlinkKbps: 0,
  uplinkKbps: 0,
  rxCodec: "",
};

export const useFmoStore = defineStore("fmo", () => {
  const state = ref<FmoStateSnapshot>(initial);
  const stats = ref<FmoStatsSnapshot>(initialStats);
  const traffic = ref<Record<string, FmoServerTraffic>>({});
  // QSO 呼叫状态 / QSO 记录 / 服务器广播配置
  const qso = ref<FmoQsoState>({ phase: "idle", peer: "", peerUid: 0, outgoing: false });
  const qsoLog = ref<FmoQsoRecord[]>([]);
  const broadcast = ref<FmoBroadcastConfig>({
    mode_min: 0, name: "", host: "", port: 1883, cover_km: 100,
    online: 0, peak: 0, country: "CN", lat: 39.9, lon: 116.4,
  });
  const bootstrapped = ref(false);
  const busy = ref(false);
  const unlisteners: UnlistenFn[] = [];
  let statsTimerId: number | null = null;

  const mqttConnected = () => state.value.mqttState === "connected";
  const selectedServer = () => state.value.selectedServer as Partial<FmoServer> | null;

  async function refreshStats() {
    try {
      stats.value = await fmoStatsSnapshot();
    } catch (e) {
      flog("[fmo] stats error:", String(e));
    }
  }

  function startStatsPolling() {
    if (statsTimerId !== null) {
      return;
    }
    void refreshStats();
    statsTimerId = window.setInterval(() => {
      void refreshStats();
    }, 1000);
  }

  function applyEvent(ev: FmoEvent) {
    switch (ev.type) {
      case "server_list":
        state.value.servers = (ev.servers as FmoServer[]) ?? [];
        break;
      case "client_list":
        state.value.clients = (ev.clients as FmoStateSnapshot["clients"]) ?? [];
        break;
      case "cert_state":
        state.value.certs = (ev.certs as FmoStateSnapshot["certs"]) ?? [];
        break;
      case "favorites":
        state.value.favorites = (ev.favorites as FmoStateSnapshot["favorites"]) ?? [];
        break;
      case "mqtt_state":
        state.value.mqttState = (ev.state as string) ?? "disconnected";
        state.value.mqttDetail = (ev.detail as string) ?? "";
        if (typeof ev.client_id === "string" && ev.client_id) {
          state.value.mqttClientId = ev.client_id;
        }
        break;
      case "aprs_state":
        state.value.aprsState = (ev.state as string) ?? "disconnected";
        state.value.aprsDetail = (ev.detail as string) ?? "";
        break;
      case "server_traffic":
        traffic.value = {
          ...traffic.value,
          [ev.host as string]: ev.traffic as unknown as FmoServerTraffic,
        };
        break;
      case "qso_state":
        qso.value = {
          phase: (ev.phase as FmoQsoState["phase"]) ?? "idle",
          peer: (ev.peer as string) ?? "",
          peerUid: (ev.peerUid as number) ?? 0,
          outgoing: (ev.outgoing as boolean) ?? false,
          detail: (ev.detail as string) ?? "",
          autoAccept: qso.value.autoAccept,
        };
        break;
      case "qso_log_changed":
        void refreshQsoLog();
        break;
      default:
        break;
    }
  }

  async function bootstrap() {
    if (bootstrapped.value) {
      return;
    }
    unlisteners.push(await onFmoEvent((ev) => {
      applyEvent(ev);
      if (ev.type === "log") {
        flog("[fmo]", ev.msg);
      }
    }));
    // 高频独立音频状态：FMO 频谱/电平实时更新（驱动 FMO box 内波形）。
    // FMO 语音按 240ms 一帧突发到达（一帧 6 个 40ms 包），直接应用会让波形约 4 次/秒跳变；
    // 排队后按 40ms 匀速铺开，与音频播放节奏一致，波形平滑。
    const applyFmoAudioState = (ev: FmoAudioState) => {
      if (typeof ev.rxLevel === "number") {
        stats.value.rxLevel = ev.rxLevel;
      }
      if (Array.isArray(ev.rxSpectrum)) {
        stats.value.rxSpectrum = ev.rxSpectrum;
      }
      if (typeof ev.txLevel === "number") {
        stats.value.txLevel = ev.txLevel;
      }
      if (Array.isArray(ev.txSpectrum)) {
        stats.value.txSpectrum = ev.txSpectrum;
      }
      if (typeof ev.jitterMs === "number") {
        stats.value.jitterMs = ev.jitterMs;
      }
      if (typeof ev.queuedFrames === "number") {
        stats.value.queuedFrames = ev.queuedFrames;
      }
      if (typeof ev.downlinkKbps === "number") {
        stats.value.downlinkKbps = ev.downlinkKbps;
      }
      if (typeof ev.rxCodec === "string") {
        stats.value.rxCodec = ev.rxCodec;
      }
      if (typeof ev.rxFrames === "number") {
        stats.value.rxFrames = ev.rxFrames;
      }
    };
    const audioStateQueue: FmoAudioState[] = [];
    let paceTimer: number | null = null;
    unlisteners.push(await onFmoAudioState((ev) => {
      audioStateQueue.push(ev);
      // 积压超过约 0.5s 则丢弃最老数据，避免显示落后实时太多
      if (audioStateQueue.length > 12) {
        audioStateQueue.splice(0, audioStateQueue.length - 12);
      }
      if (paceTimer === null) {
        paceTimer = window.setInterval(() => {
          const next = audioStateQueue.shift();
          if (next === undefined) {
            if (paceTimer !== null) {
              window.clearInterval(paceTimer);
              paceTimer = null;
            }
            return;
          }
          applyFmoAudioState(next);
        }, 40);
      }
    }));
    await refresh();
    try {
      qso.value = await fmoQsoState();
      qsoLog.value = await fmoQsoLog();
      broadcast.value = await fmoBroadcastConfig();
    } catch (e) {
      flog("[fmo] qso/broadcast init error:", String(e));
    }
    bootstrapped.value = true;
    startStatsPolling();
  }

  /** 重新拉取 FMO 完整快照（证书导入后调用，刷新身份/呼号/UID/passcode/服务器）。 */
  async function refresh() {
    try {
      state.value = await fmoStateSnapshot();
      flog("[fmo] refresh identity:", state.value.identity, "passcode:", state.value.passcode);
    } catch (e) {
      flog("[fmo] refresh error:", String(e));
    }
  }

  async function runAction(action: () => Promise<unknown>) {
    if (busy.value) {
      return;
    }
    busy.value = true;
    try {
      await action();
    } catch (e) {
      flog("[fmo] action error:", String(e));
      throw e;
    } finally {
      busy.value = false;
    }
  }

  async function connectAprs(callsign: string) {
    const passcode = state.value.passcode || "-1";
    await runAction(() => fmoAprsConnect(callsign, passcode));
  }

  async function disconnectAprs() {
    await runAction(fmoAprsDisconnect);
  }

  async function selectServer(server: FmoServer) {
    state.value.selectedServer = server;
    await runAction(() => fmoServerSelect(server as unknown as Record<string, unknown>));
  }

  async function connectMqtt() {
    await runAction(() => fmoMqttConnect(false));
  }

  async function disconnectMqtt() {
    await runAction(fmoMqttDisconnect);
  }

  async function importCert(name: string, cert: Record<string, unknown>) {
    await runAction(() => fmoCertImportJson(name, cert));
  }

  async function importCertFile(filePath: string, name?: string) {
    await runAction(() => fmoCertImportFile(filePath, name));
  }

  async function addFavorite(server: FmoServer) {
    await runAction(() =>
      fmoFavoritesAdd(server as unknown as Record<string, unknown>),
    );
  }

  async function removeFavorite(key: string) {
    await runAction(() => fmoFavoritesRemove(key));
  }

  async function setRxPlay(enabled: boolean) {
    state.value.rxPlay = enabled;
    await runAction(() => fmoRxPlay(enabled));
  }

  async function setMqttNoLocal(enabled: boolean) {
    state.value.mqttNoLocal = enabled;
    await runAction(() => fmoMqttNoLocal(enabled));
  }

  async function refreshQsoLog() {
    try {
      qsoLog.value = await fmoQsoLog();
    } catch (e) {
      flog("[fmo] qso log error:", String(e));
    }
  }

  async function qsoCall(target: string, uid?: number) {
    await runAction(() => fmoQsoCall(target, uid));
  }

  async function qsoAnswer(accept: boolean) {
    await runAction(() => fmoQsoAnswer(accept));
  }

  async function qsoCancel() {
    await runAction(fmoQsoCancel);
  }

  async function setQsoAutoAccept(enabled: boolean) {
    qso.value = { ...qso.value, autoAccept: enabled };
    await runAction(() => fmoQsoSetAutoAccept(enabled));
  }

  async function saveBroadcast(cfg: FmoBroadcastConfig) {
    broadcast.value = { ...cfg };
    await runAction(() => fmoBroadcastSetConfig(cfg));
  }

  async function broadcastNow() {
    await runAction(fmoBroadcastNow);
  }

  return {
    state,
    stats,
    traffic,
    qso,
    qsoLog,
    broadcast,
    bootstrapped,
    busy,
    mqttConnected,
    selectedServer,
    bootstrap,
    refresh,
    refreshStats,
    connectAprs,
    disconnectAprs,
    selectServer,
    connectMqtt,
    disconnectMqtt,
    importCert,
    importCertFile,
    addFavorite,
    removeFavorite,
    setRxPlay,
    setMqttNoLocal,
    refreshQsoLog,
    qsoCall,
    qsoAnswer,
    qsoCancel,
    setQsoAutoAccept,
    saveBroadcast,
    broadcastNow,
  };
});
