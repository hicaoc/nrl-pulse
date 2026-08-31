<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { usePlatformStore } from "@/stores/platform";
import { useRuntimeStore } from "@/stores/runtime";
import { platformSwitchGroup } from "@/lib/platform";

interface WsSpeaker {
  callsign: string;
  ssid: number;
}

interface WsRoom {
  room_key: string;
  room_id: number;
  room_name: string;
  room_type: number;
  online_dev_number: number;
  total_dev_number: number;
  callsign?: string;
  ssid?: number;
  speakers?: WsSpeaker[];
  active?: boolean;
  updated_at?: string;
  last_active?: number;
  speakersText?: string;
  typeName?: string;
  online?: number;
  isCurrent?: boolean;
}

const PING_INTERVAL_MS = 10_000;
const RECONNECT_DELAY_MS = 2_000;
const SAMPLE_RATE = 8_000;

const ROOM_TYPE_NAMES: Record<number, string> = {
  0: "公共",
  1: "中继互联",
  2: "设备互联",
  4: "数模互联",
  5: "俱乐部",
  6: "车友会",
  7: "会议组",
  8: "私密房间",
  100: "其他",
};

const platform = usePlatformStore();
const runtime = useRuntimeStore();

const connected = ref(false);
const connecting = ref(true);
const errorText = ref("");
const rooms = ref<WsRoom[]>([]);
const subscribedKeys = ref<Record<string, boolean>>({});
const stats = ref({ totalSubs: 0, connectedClients: 0, onlineDevices: 0 });
const currentGroupId = ref(0);
const joinBusy = ref(false);
const actionMessage = ref("");
const audioEnabled = ref(true);

const language = ref((localStorage.getItem("nrl-pulse-lang") as "zh" | "en") || "zh");
const text = computed(() => {
  const zh = language.value === "zh";
  return {
    title: zh ? "房间监听" : "Room Monitor",
    server: zh ? "服务器" : "Server",
    online: zh ? "在线设备" : "Online Devices",
    listeners: zh ? "收听" : "Listeners",
    clients: zh ? "连接" : "Clients",
    authTip: zh ? "监听不需要登录。未登录仅显示公共房间；登录后可看到并收听当前账号有权限的房间。" : "No sign-in required. Anonymous users see public rooms; signed-in users also see authorized rooms.",
    empty: zh ? "暂无房间" : "No rooms",
    connecting: zh ? "正在连接服务器..." : "Connecting...",
    listen: zh ? "收听" : "Listen",
    listening: zh ? "收听中" : "Listening",
    current: zh ? "当前群组" : "Current",
    join: zh ? "加入" : "Join",
    joined: zh ? "已加入" : "Joined",
    loginRequired: zh ? "加入群组需要先登录当前服务器" : "Sign in to join a group",
    audioOn: zh ? "音频开" : "Audio On",
    audioOff: zh ? "音频关" : "Audio Off",
    close: zh ? "关闭" : "Close",
    joinFailed: zh ? "加入失败" : "Join failed",
    joinedPrefix: zh ? "已加入" : "Joined",
  };
});

const serverName = computed(() =>
  runtime.config.serverName || runtime.config.server || "-",
);
const myCallsign = computed(() => runtime.config.callsign.toUpperCase());

let websocket: WebSocket | null = null;
let pingTimer: number | null = null;
let reconnectTimer: number | null = null;
let destroyed = false;
let roomsByKey: Record<string, WsRoom> = {};

let audioContext: AudioContext | null = null;
let audioGain: GainNode | null = null;
let nextAudioTime = 0;

function isLocalHost(host: string): boolean {
  return /^(localhost|127\.0\.0\.1|::1|\[::1\])$/i.test(host);
}

function wsBaseUrl(value: string): string {
  const raw = value.trim();
  if (!raw) return "";
  try {
    if (/^https?:\/\//i.test(raw)) {
      const url = new URL(raw);
      return `${url.protocol === "https:" ? "wss:" : "ws:"}//${url.host}`;
    }
    if (/^wss?:\/\//i.test(raw)) {
      const url = new URL(raw);
      return `${url.protocol}//${url.host}`;
    }
  } catch {
    // fall through to plain-host handling
  }

  const host = raw.replace(/^\/+/, "").replace(/\/+$/, "");
  if (/^https:\/\//i.test(host)) return `wss://${host.replace(/^https:\/\//, "")}`;
  if (isLocalHost(host)) return `ws://${host}`;
  return `wss://${host}`;
}

function speakersText(room: WsRoom): string {
  const speakers = room.speakers?.length
    ? room.speakers
    : room.callsign
      ? [{ callsign: room.callsign, ssid: room.ssid ?? 0 }]
      : [];
  return speakers.map((item) => `${item.callsign}-${item.ssid}`).join(" / ");
}

function normalizeRooms(): void {
  const privatePrefix = `private:${myCallsign.value}:`;
  rooms.value = Object.values(roomsByKey)
    .map((room) => {
      const key = String(room.room_key || "");
      const isPrivate = key.startsWith("private:");
      const isCurrent = room.room_id === currentGroupId.value &&
        (!isPrivate || key.startsWith(privatePrefix));
      return {
        ...room,
        speakersText: speakersText(room),
        typeName: ROOM_TYPE_NAMES[room.room_type] || "群组",
        online: room.online_dev_number || 0,
        isCurrent,
      };
    })
    .sort((a, b) => a.room_id - b.room_id || a.room_key.localeCompare(b.room_key));
}

function applySubscriptions(keys: string[]): void {
  const map: Record<string, boolean> = {};
  keys.forEach((key) => {
    map[key] = true;
  });
  subscribedKeys.value = map;
}

function applyStats(message: any): void {
  stats.value = {
    totalSubs: Number(message.total_subs || 0),
    connectedClients: Number(message.connected_clients || 0),
    onlineDevices: Number(message.online_devices || 0),
  };
}

function handleMessage(message: any): void {
  switch (message.type) {
    case "snapshot":
      roomsByKey = {};
      (message.rooms || []).forEach((room: WsRoom) => {
        roomsByKey[room.room_key] = room;
      });
      applySubscriptions(message.subscriptions || []);
      applyStats(message);
      normalizeRooms();
      break;
    case "rooms":
      roomsByKey = {};
      (message.rooms || []).forEach((room: WsRoom) => {
        roomsByKey[room.room_key] = room;
      });
      normalizeRooms();
      break;
    case "room_state":
      if (message.room?.room_key) {
        roomsByKey[message.room.room_key] = {
          ...(roomsByKey[message.room.room_key] || {}),
          ...message.room,
        };
        normalizeRooms();
      }
      break;
    case "subscriptions":
      applySubscriptions(message.subscriptions || []);
      break;
    case "stats":
      applyStats(message);
      break;
    default:
      break;
  }
}

function alawToLinear(value: number): number {
  let byte = value ^ 0x55;
  const sign = byte & 0x80;
  const exponent = (byte & 0x70) >> 4;
  const mantissa = byte & 0x0f;
  const sample = exponent
    ? ((mantissa << 4) + 0x108) << (exponent - 1)
    : (mantissa << 4) + 8;
  return sign ? -sample : sample;
}

function ensureAudioContext(): void {
  if (!audioContext) {
    audioContext = new AudioContext({ sampleRate: SAMPLE_RATE });
    audioGain = audioContext.createGain();
    audioGain.gain.value = 1;
    audioGain.connect(audioContext.destination);
  }
  if (audioContext.state === "suspended") {
    void audioContext.resume();
  }
}

function playAudioFrame(bytes: Uint8Array): void {
  if (!audioEnabled.value || !bytes.length) return;
  ensureAudioContext();
  if (!audioContext || !audioGain) return;

  const samples = new Int16Array(bytes.length);
  for (let i = 0; i < bytes.length; i += 1) {
    samples[i] = alawToLinear(bytes[i]);
  }

  const buffer = audioContext.createBuffer(1, samples.length, SAMPLE_RATE);
  const channel = buffer.getChannelData(0);
  for (let i = 0; i < samples.length; i += 1) {
    channel[i] = samples[i] / 32768;
  }

  const source = audioContext.createBufferSource();
  source.buffer = buffer;
  source.connect(audioGain);
  const now = audioContext.currentTime;
  if (nextAudioTime < now) {
    nextAudioTime = now + 0.02;
  }
  if (nextAudioTime > now + 0.45) {
    nextAudioTime = now + 0.02;
  }
  source.start(nextAudioTime);
  nextAudioTime += samples.length / SAMPLE_RATE;
}

function sendCommand(command: Record<string, unknown>): void {
  if (websocket && websocket.readyState === WebSocket.OPEN) {
    websocket.send(JSON.stringify(command));
  }
}

function startPing(): void {
  stopPing();
  pingTimer = window.setInterval(() => {
    sendCommand({ action: "ping" });
  }, PING_INTERVAL_MS);
}

function stopPing(): void {
  if (pingTimer != null) {
    window.clearInterval(pingTimer);
    pingTimer = null;
  }
}

function scheduleReconnect(): void {
  if (destroyed || reconnectTimer != null) return;
  reconnectTimer = window.setTimeout(() => {
    reconnectTimer = null;
    connect();
  }, RECONNECT_DELAY_MS);
}

function closeSocket(): void {
  if (websocket) {
    const socket = websocket;
    websocket = null;
    socket.onopen = null;
    socket.onmessage = null;
    socket.onclose = null;
    socket.onerror = null;
    try {
      socket.close();
    } catch {
      // ignore
    }
  }
}

function connect(): void {
  if (destroyed) return;
  closeSocket();
  stopPing();
  connecting.value = true;
  connected.value = false;

  const host = normalizeWsHost(runtime.config.server || runtime.config.apiBase || "");
  if (!host) {
    connecting.value = false;
    errorText.value = language.value === "zh" ? "未配置 NRL 服务器" : "No NRL server configured";
    return;
  }

  const token = platform.token || "";
  const url = `${wsBaseUrl(host)}/ws/calls${token ? `?token=${encodeURIComponent(token)}` : ""}`;
  const socket = new WebSocket(url);
  socket.binaryType = "arraybuffer";
  websocket = socket;

  socket.onopen = () => {
    connected.value = true;
    connecting.value = false;
    errorText.value = "";
    startPing();
  };
  socket.onmessage = (event) => {
    if (typeof event.data === "string") {
      try {
        handleMessage(JSON.parse(event.data));
      } catch {
        // ignore invalid JSON
      }
      return;
    }
    const bytes = event.data instanceof ArrayBuffer
      ? new Uint8Array(event.data)
      : new Uint8Array([]);
    playAudioFrame(bytes);
  };
  socket.onclose = () => {
    connected.value = false;
    connecting.value = false;
    stopPing();
    scheduleReconnect();
  };
  socket.onerror = () => {
    connecting.value = false;
    errorText.value = language.value === "zh" ? "WebSocket 连接失败" : "WebSocket connection failed";
  };
}

function normalizeWsHost(value: string): string {
  const base = wsBaseUrl(value);
  return base.replace(/^ws[s]?:\/\//, "");
}

function toggleRoom(room: WsRoom): void {
  const key = room.room_key;
  if (!key) return;
  const subscribed = !!subscribedKeys.value[key];
  if (!subscribed) {
    audioEnabled.value = true;
    ensureAudioContext();
  }
  sendCommand({
    action: subscribed ? "unsubscribe" : "subscribe",
    room_keys: [key],
  });
}

function toggleAudio(): void {
  audioEnabled.value = !audioEnabled.value;
  if (audioEnabled.value) ensureAudioContext();
  else audioContext?.suspend();
}

async function joinRoom(room: WsRoom): Promise<void> {
  if (!room || joinBusy.value) return;
  if (!platform.loggedIn) {
    actionMessage.value = text.value.loginRequired;
    return;
  }

  joinBusy.value = true;
  actionMessage.value = "";
  try {
    const snapshot = await platformSwitchGroup(
      runtime.config.apiBase,
      platform.token,
      runtime.config.callsign,
      runtime.config.ssid,
      room.room_id,
    );
    currentGroupId.value = snapshot.currentGroupId;
    await runtime.saveConfig({
      ...runtime.config,
      currentGroupId: snapshot.currentGroupId,
      roomName: room.room_name || runtime.config.roomName,
    });
    actionMessage.value = `${text.value.joinedPrefix} ${room.room_name || room.room_id}`;
  } catch (error) {
    actionMessage.value = error instanceof Error ? error.message : text.value.joinFailed;
  } finally {
    joinBusy.value = false;
  }
}

onMounted(async () => {
  document.documentElement.classList.add("monitor-window");
  document.body.classList.add("monitor-window");
  try {
    await runtime.bootstrap();
    // 登录是可选的：失败或未登录时仍应连接当前 NRL 服务器的匿名 WS。
    try {
      await platform.bootstrap();
    } catch {
      // ignore: anonymous listening remains available
    }
    currentGroupId.value = runtime.config.currentGroupId || platform.currentGroupId?.id || 0;
    connect();
  } catch (error) {
    connecting.value = false;
    errorText.value = error instanceof Error ? error.message : String(error);
  }
});

watch(
  () => runtime.config.server,
  (nextServer, previousServer) => {
    const next = normalizeWsHost(nextServer || "");
    const previous = normalizeWsHost(previousServer || "");
    if (next && next !== previous) {
      connect();
    }
  },
);

onBeforeUnmount(() => {
  destroyed = true;
  stopPing();
  closeSocket();
  if (reconnectTimer != null) {
    window.clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
  audioContext?.close();
  audioContext = null;
  audioGain = null;
});
</script>

<template>
  <main class="monitor-shell">
    <header class="monitor-topbar">
      <div class="monitor-title-wrap">
        <span class="monitor-led" :data-state="connected ? 'on' : 'off'"></span>
        <div>
          <h1>{{ text.title }}</h1>
          <span>{{ serverName }}</span>
        </div>
      </div>
      <div class="monitor-stats">
        <span>{{ text.online }} {{ stats.onlineDevices }}</span>
        <span>{{ text.listeners }} {{ stats.totalSubs }}</span>
        <span>{{ text.clients }} {{ stats.connectedClients }}</span>
        <button class="ghost-btn compact" @click="toggleAudio">
          {{ audioEnabled ? text.audioOn : text.audioOff }}
        </button>
      </div>
    </header>

    <div v-if="!platform.loggedIn" class="monitor-auth-tip">{{ text.authTip }}</div>
    <div v-if="errorText" class="auth-error monitor-error">{{ errorText }}</div>

    <div v-if="!rooms.length" class="monitor-empty">
      {{ connected ? text.empty : text.connecting }}
    </div>

    <div v-else class="monitor-grid">
      <article
        v-for="room in rooms"
        :key="room.room_key"
        class="monitor-room"
        :data-room-type="room.room_type"
        :data-speaking="room.active ? 'true' : 'false'"
        :data-subscribed="subscribedKeys[room.room_key] ? 'true' : 'false'"
        :data-current="room.isCurrent ? 'true' : 'false'"
      >
        <div class="monitor-room-head">
          <strong>#{{ room.room_id }} {{ room.room_name }}</strong>
        </div>
        <div class="monitor-room-meta">
          <span class="monitor-room-type">{{ room.typeName }}</span>
          <span class="monitor-room-state" :data-active="room.active ? 'true' : 'false'">
            {{ room.online }} · {{ room.active ? room.speakersText : (language === "zh" ? "空闲" : "Idle") }}
          </span>
        </div>
        <div class="monitor-room-foot">
          <button
            class="monitor-btn listen"
            :data-on="subscribedKeys[room.room_key] ? 'true' : 'false'"
            @click="toggleRoom(room)"
          >
            {{ subscribedKeys[room.room_key] ? text.listening : text.listen }}
          </button>
          <span v-if="room.isCurrent" class="monitor-current">{{ text.current }}</span>
          <button
            v-else
            class="monitor-btn join"
            :disabled="!platform.loggedIn || joinBusy"
            @click="joinRoom(room)"
          >
            {{ text.join }}
          </button>
        </div>
      </article>
    </div>
  </main>
</template>

<style scoped>
.monitor-shell {
  width: 100%;
  min-height: 100vh;
  padding: 18px;
  overflow: auto;
  background: #05080f;
  color: #e8f4ff;
  box-sizing: border-box;
}

.monitor-topbar {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 14px;
  padding: 14px 16px;
  border: 1px solid rgba(255, 255, 255, 0.08);
  border-radius: 16px;
  background: rgba(255, 255, 255, 0.035);
}

.monitor-title-wrap {
  display: flex;
  align-items: center;
  gap: 10px;
  min-width: 0;
}

.monitor-title-wrap h1 {
  margin: 0;
  font-size: 17px;
}

.monitor-title-wrap span {
  display: block;
  font-size: 12px;
  color: rgba(233, 244, 255, 0.6);
}

.monitor-led {
  width: 10px;
  height: 10px;
  border-radius: 50%;
}

.monitor-led[data-state="on"] {
  background: #22c55e;
  box-shadow: 0 0 12px rgba(34, 197, 94, 0.7);
}

.monitor-led[data-state="off"] {
  background: #64748b;
}

.monitor-stats {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
  font-size: 12px;
  color: rgba(233, 244, 255, 0.65);
}

.monitor-auth-tip,
.monitor-error {
  margin-top: 12px;
  padding: 10px 12px;
  border-radius: 12px;
  font-size: 12px;
}

.monitor-auth-tip {
  color: #ffd166;
  background: rgba(255, 209, 102, 0.1);
  border: 1px solid rgba(255, 209, 102, 0.22);
}

.monitor-error {
  color: #ff8f8f;
  background: rgba(255, 113, 113, 0.1);
  border: 1px solid rgba(255, 113, 113, 0.22);
}

.monitor-empty {
  display: grid;
  place-items: center;
  min-height: 240px;
  color: rgba(233, 244, 255, 0.55);
}

.monitor-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(190px, 1fr));
  gap: 12px;
  margin-top: 14px;
}

.monitor-room {
  min-width: 0;
  padding: 13px;
  border: 1px solid rgba(255, 255, 255, 0.08);
  border-radius: 14px;
  background: rgba(255, 255, 255, 0.03);
  transition: all 0.16s ease;
}

.monitor-room[data-room-type="1"] { background: rgba(148, 102, 255, 0.09); }
.monitor-room[data-room-type="2"] { background: rgba(34, 197, 94, 0.07); }
.monitor-room[data-room-type="4"] { background: rgba(91, 192, 255, 0.08); }
.monitor-room[data-room-type="5"] { background: rgba(255, 169, 77, 0.09); }
.monitor-room[data-room-type="7"] { background: rgba(255, 105, 135, 0.08); }
.monitor-room[data-room-type="8"] { background: rgba(148, 163, 184, 0.08); }

.monitor-room[data-speaking="true"] {
  border-color: rgba(34, 197, 94, 0.85);
  background:
    radial-gradient(circle at 50% 0%, rgba(34, 197, 94, 0.16), transparent 70%),
    rgba(34, 197, 94, 0.08);
  box-shadow:
    0 0 0 1px rgba(34, 197, 94, 0.22),
    0 0 18px rgba(34, 197, 94, 0.22),
    0 0 42px rgba(34, 197, 94, 0.14);
  animation: monitor-speaking-glow 1.35s ease-in-out infinite;
}

@keyframes monitor-speaking-glow {
  0%,
  100% {
    border-color: rgba(34, 197, 94, 0.55);
    box-shadow:
      0 0 0 1px rgba(34, 197, 94, 0.16),
      0 0 12px rgba(34, 197, 94, 0.14),
      0 0 30px rgba(34, 197, 94, 0.07);
    background:
      radial-gradient(circle at 50% 0%, rgba(34, 197, 94, 0.10), transparent 70%),
      rgba(34, 197, 94, 0.04);
  }

  50% {
    border-color: rgba(34, 197, 94, 0.95);
    box-shadow:
      0 0 0 1px rgba(34, 197, 94, 0.30),
      0 0 22px rgba(34, 197, 94, 0.30),
      0 0 54px rgba(34, 197, 94, 0.20);
    background:
      radial-gradient(circle at 50% 0%, rgba(34, 197, 94, 0.20), transparent 70%),
      rgba(34, 197, 94, 0.10);
  }
}

.monitor-room[data-subscribed="true"] {
  border-color: rgba(91, 192, 255, 0.6);
}

.monitor-room[data-current="true"] {
  border-color: rgba(34, 197, 94, 0.75);
}

.monitor-room-head strong {
  display: block;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: 14px;
}

.monitor-room-meta {
  display: flex;
  justify-content: space-between;
  gap: 8px;
  margin: 8px 0 10px;
  font-size: 11px;
  color: rgba(233, 244, 255, 0.58);
}

.monitor-room-type {
  flex-shrink: 0;
  padding: 2px 6px;
  border-radius: 6px;
  background: rgba(255, 255, 255, 0.06);
}

.monitor-room-state {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.monitor-room-state[data-active="true"] {
  color: #4ade80;
  font-weight: 700;
}

.monitor-room-foot {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}

/* 收听/加入按钮默认隐藏；悬停或键盘聚焦房间时显示。
   已订阅的“收听中”保持可见，方便确认当前状态。 */
.monitor-room-foot .monitor-btn {
  opacity: 0;
  visibility: hidden;
  transform: translateY(2px);
  pointer-events: none;
  transition: opacity 0.15s ease, visibility 0.15s ease, transform 0.15s ease;
}

.monitor-room:hover .monitor-btn,
.monitor-room:focus-within .monitor-btn,
.monitor-room-foot .monitor-btn[data-on="true"] {
  opacity: 1;
  visibility: visible;
  transform: translateY(0);
  pointer-events: auto;
}

.monitor-btn {
  border: 1px solid transparent;
  border-radius: 9px;
  padding: 5px 12px;
  font-size: 12px;
  cursor: pointer;
}

.monitor-btn.join {
  color: #04120a;
  background: #22c55e;
}

.monitor-btn.join:disabled {
  color: #a3b2c2;
  background: rgba(255, 255, 255, 0.07);
  cursor: not-allowed;
}

.monitor-btn.listen {
  color: #9edcff;
  background: rgba(91, 192, 255, 0.08);
  border-color: rgba(91, 192, 255, 0.42);
}

.monitor-btn.listen[data-on="true"] {
  color: #cdeaff;
  background: rgba(91, 192, 255, 0.13);
  border-color: rgba(91, 192, 255, 0.34);
  font-weight: 600;
}

.monitor-current {
  margin-left: auto;
  font-size: 11px;
  color: #4ade80;
}

@media (max-width: 720px) {
  .monitor-shell {
    padding: 12px;
  }
  .monitor-topbar {
    align-items: flex-start;
    flex-direction: column;
  }
  .monitor-grid {
    grid-template-columns: 1fr;
  }
}
</style>
