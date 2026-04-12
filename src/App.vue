<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { getVersion } from "@tauri-apps/api/app";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  checkUpdate,
  closePttWindow,
  downloadAndInstallUpdate,
  flog,
  onChatMessage,
  openPttWindow,
  startPttWindowDrag,
  togglePttWindow,
} from "@/lib/tauri";
import type { UpdateInfo } from "@/lib/tauri";
import { usePlatformStore } from "@/stores/platform";
import { useRuntimeStore } from "@/stores/runtime";
import type { ChatMessageEvent } from "@/types";

type Lang = "zh" | "en";

const HOLD_THRESHOLD_MS = 320;
const runtime = useRuntimeStore();
const platform = usePlatformStore();
const isPttWindow = window.location.hash === "#ptt";

const draftMessage = ref("");
const pttKeyDraft = ref("Space");
const showSettings = ref(false);
const showLogs = ref(false);
const updateInfo = ref<UpdateInfo | null>(null);
const updateDownloading = ref(false);
const updateProgress = ref(0);
const updateTotal = ref(0);
const showLogin = ref(false);
const loginError = ref("");
const listeningPttKey = ref(false);
const pttPressed = ref(false);
const holdActivated = ref(false);
const holdTimerId = ref<number | null>(null);
const animationTimerId = ref<number | null>(null);
const animationTick = ref(0);
const language = ref<Lang>((localStorage.getItem("nrl-pulse-lang") as Lang) || "zh");
const chatMessages = ref<
  ChatMessageEvent[]
>([]);

const messages = {
  zh: {
    language: "中文",
    session: "会话",
    currentGroup: "当前组",
    latency: "延迟",
    jitter: "抖动",
    loss: "丢包",
    queue: "队列",
    receive: "接收",
    transmit: "发射",
    platformAccount: "平台账号",
    platformLogin: "登录",
    platformLoggedIn: "已登录",
    systemLogs: "日志",
    closeLogs: "关闭日志",
    openSettings: "配置",
    closeSettings: "关闭参数",
    currentSession: "当前会话",
    onAir: "在空中",
    connected: "已连接",
    connecting: "连接中",
    disconnected: "离线",
    recovering: "恢复中",
    connect: "连接",
    disconnect: "断开",
    enableMute: "开启静音",
    disableMute: "取消静音",
    stopRecording: "停止录音",
    startRecording: "开始录音",
    pttWindow: "PTT",
    currentTalker: "当前发言",
    regionUnknown: "区域未识别",
    groupNotSelected: "未选择群组",
    online: "在线",
    groupSwitch: "群组切换",
    groupSearch: "搜索名称或 ID…",
    refresh: "刷新",
    loginFirstRoom: "先登录平台账号，再选择服务器和房间。",
    openLogin: "打开登录",
    onlineDevices: "在线设备",
    loginToSeeDevices: "登录后这里显示当前组的在线设备列表。",
    noOnlineDevices: "当前组暂无在线设备。",
    onlineDevice: "在线设备",
    onlineBadge: "在线",
    commandText: "消息",
    messagesCount: (count: number) => `${count} 条`,
    noMessages: "暂无调度消息，收到文本后会显示在这里。",
    messagePlaceholder: "输入调度消息",
    currentStatus: "当前状态",
    sendMessage: "发送",
    linkParams: "链路参数",
    deviceJitter: "设备与抖动缓冲",
    close: "关闭",
    closePttWindow: "关闭 PTT 悬浮窗",
    ssidDevMode: "SSID / DevMode",
    voiceCallsign: "语音呼号",
    pttHotkey: "PTT 热键",
    anyKey: "按下任意键",
    selectedPlatform: "已选平台",
    currentRoom: "当前房间",
    unselected: "未选择",
    saveLocalSettings: "保存本地设置",
    setPttKey: "设置 PTT 键",
    waitKey: "等待按键",
    syncAt: "同步 AT 状态",
    localCallsign: "本机呼号",
    inputDevice: "输入设备",
    outputDevice: "输出设备",
    sampleRate: "采样率",
    jitterBuffer: "抖动缓冲",
    agc: "AGC",
    noiseSuppression: "降噪",
    aec: "回声消除 AEC",
    aecUnsupported: "当前系统不支持回声消除",
    enabled: "开启",
    disabled: "关闭",
    runningLogs: "运行日志",
    systemLog: "系统日志",
    noLogs: "暂无系统日志，连接和收发状态会显示在这里。",
    platformAuth: "平台账号",
    serverLogin: "平台登录",
    loginServer: "登录服务器",
    voicePort: "语音端口",
    username: "用户名",
    password: "密码",
    loggingIn: "登录中...",
    relogin: "重新登录",
    loginPlatformAction: "登录平台",
    refreshServers: "刷新服务器列表",
    currentAccount: "当前账号",
    currentGroupLabel: "当前组",
    logoutLocal: "退出本地登录态",
    pttHint: (key: string) => `短按切换发射，长按保持发射，松开结束。键盘触发键：${key}`,
    ptt: "PTT",
    txActive: "发射中",
    txIdle: "待发射",
    updateAvailable: (v: string) => `发现新版本 ${v}，点击更新`,
    updateDownloading: "下载中...",
    updateNone: "当前已是最新版本",
    checkUpdate: "检查更新",
    mute: "静音",
    recording: "录音",
    roomWithOnline: (name: string, onlineCount: number, totalCount: number) =>
      `${name} · 在线 ${onlineCount}/${totalCount}`,
    zone: (value: string) => `${value} 区`,
  },
  en: {
    language: "EN",
    session: "Session",
    currentGroup: "Group",
    latency: "Latency",
    jitter: "Jitter",
    loss: "Loss",
    queue: "Queue",
    receive: "RX",
    transmit: "TX",
    platformAccount: "Account",
    platformLogin: "Login",
    platformLoggedIn: "Logged In",
    systemLogs: "Logs",
    closeLogs: "Hide Logs",
    openSettings: "Settings",
    closeSettings: "Hide Settings",
    currentSession: "Current Session",
    onAir: "On Air",
    connected: "Connected",
    connecting: "Connecting",
    disconnected: "Offline",
    recovering: "Recovering",
    connect: "Connect",
    disconnect: "Disconnect",
    enableMute: "Mute",
    disableMute: "Unmute",
    stopRecording: "Stop Recording",
    startRecording: "Start Recording",
    pttWindow: "PTT Window",
    currentTalker: "Current Talker",
    regionUnknown: "Region Unknown",
    groupNotSelected: "No Group Selected",
    online: "Online",
    groupSwitch: "Group Switch",
    groupSearch: "Search name or ID…",
    refresh: "Refresh",
    loginFirstRoom: "Log in to the platform account first, then choose a room.",
    openLogin: "Open Login",
    onlineDevices: "Online Devices",
    loginToSeeDevices: "Online devices for the current group appear here after login.",
    noOnlineDevices: "No online devices in this group.",
    onlineDevice: "Online Device",
    onlineBadge: "Online",
    commandText: "Messages",
    messagesCount: (count: number) => `${count}`,
    noMessages: "No text messages yet.",
    messagePlaceholder: "Type a dispatch message",
    currentStatus: "Status",
    sendMessage: "Send",
    linkParams: "Link Parameters",
    deviceJitter: "Devices & Jitter Buffer",
    close: "Close",
    closePttWindow: "Close PTT Window",
    ssidDevMode: "SSID / DevMode",
    voiceCallsign: "Voice Callsign",
    pttHotkey: "PTT Hotkey",
    anyKey: "Press any key",
    selectedPlatform: "Platform",
    currentRoom: "Current Room",
    unselected: "Not Selected",
    saveLocalSettings: "Save Local Settings",
    setPttKey: "Set PTT Key",
    waitKey: "Waiting Key",
    syncAt: "Sync AT",
    localCallsign: "Local Callsign",
    inputDevice: "Input",
    outputDevice: "Output",
    sampleRate: "Sample Rate",
    jitterBuffer: "Jitter Buffer",
    agc: "AGC",
    noiseSuppression: "Noise Reduction",
    aec: "Echo Cancel AEC",
    aecUnsupported: "Echo cancellation not supported on this system",
    enabled: "On",
    disabled: "Off",
    runningLogs: "Runtime Logs",
    systemLog: "System Log",
    noLogs: "No system logs yet.",
    platformAuth: "Platform Account",
    serverLogin: "Platform Login",
    loginServer: "Server",
    voicePort: "Voice Port",
    username: "Username",
    password: "Password",
    loggingIn: "Signing In...",
    relogin: "Sign In Again",
    loginPlatformAction: "Sign In",
    refreshServers: "Refresh Servers",
    currentAccount: "Account",
    currentGroupLabel: "Current Group",
    logoutLocal: "Clear Local Session",
    pttHint: (key: string) => `Tap to toggle TX, hold to talk, release to stop. Hotkey: ${key}`,
    ptt: "PTT",
    txActive: "Transmitting",
    txIdle: "Standby",
    updateAvailable: (v: string) => `New version ${v} available, click to update`,
    updateDownloading: "Downloading...",
    updateNone: "You are on the latest version",
    checkUpdate: "Check for Updates",
    mute: "Mute",
    recording: "Record",
    roomWithOnline: (name: string, onlineCount: number, totalCount: number) =>
      `${name} · ${onlineCount}/${totalCount} online`,
    zone: (value: string) => `Zone ${value}`,
  },
} as const;

const t = computed(() => messages[language.value]);

const txLabel = computed(() => {
  if (runtime.snapshot.activeSpeaker) return `${runtime.snapshot.activeSpeaker}-${runtime.snapshot.activeSpeakerSsid}`;
  if (runtime.snapshot.isTransmitting) return t.value.txActive;
  return t.value.txIdle;
});
const pttStatusReason = computed(() => {
  if (isPttWindow) {
    flog("[ptt] pttStatusReason check: busy=", runtime.busy, "connection=", runtime.snapshot.connection);
  }
  if (runtime.busy) return "忙碌中";
  if (runtime.snapshot.connection !== "connected") return "未连接";
  return "";
});
const connectionLabel = computed(() => {
  const map = {
    connected: t.value.connected,
    connecting: t.value.connecting,
    disconnected: t.value.disconnected,
    recovering: t.value.recovering,
  } as const;
  return map[runtime.snapshot.connection];
});
const currentTalker = computed(() => {
  if (runtime.snapshot.connection !== "connected") {
    return "-";
  }
  if (runtime.snapshot.isTransmitting) {
    return `${runtime.snapshot.callsign}-${runtime.snapshot.ssid}`;
  }
  if (!runtime.snapshot.activeSpeaker) {
    return "---------";
  }
  return `${runtime.snapshot.activeSpeaker}-${runtime.snapshot.activeSpeakerSsid}`;
});
const currentTalkerRegion = computed(() => {
  if (runtime.snapshot.connection !== "connected") {
    return t.value.regionUnknown;
  }
  if (runtime.snapshot.isTransmitting) {
    return describeCallsignRegion(runtime.snapshot.callsign);
  }
  if (!runtime.snapshot.activeSpeaker) {
    return t.value.regionUnknown;
  }
  return describeCallsignRegion(runtime.snapshot.activeSpeaker);
});
const pttKeyLabel = computed(() => normalizeKeyLabel(runtime.config.pttKey));
const selectedLoginServer = computed(
  () => platform.servers.find((server) => server.host === platform.selectedServerHost) ?? null,
);
const groupSearch = ref("");
const filteredGroups = computed(() => {
  const q = groupSearch.value.trim().toLowerCase();
  if (!q) return platform.groups;
  return platform.groups.filter(
    (g) => g.name.toLowerCase().includes(q) || String(g.id).includes(q),
  );
});
const currentGroupText = computed(() => {
  if (!platform.currentGroup) {
    return t.value.groupNotSelected;
  }
  return t.value.roomWithOnline(
    platform.currentGroup.name,
    platform.currentGroup.onlineDevNumber ?? 0,
    platform.currentGroup.totalDevNumber ?? 0,
  );
});
const spectrumBars = computed(() => {
  const source = runtime.snapshot.isTransmitting
    ? runtime.snapshot.txSpectrum
    : runtime.snapshot.rxSpectrum;
  return Array.from({ length: 28 }, (_, index) => {
    const base = source[index] ?? 0;
    const shimmer = (Math.sin(animationTick.value * 0.16 + index * 0.52) + 1) * 0.025;
    return Math.min(1, Math.max(0.04, base + shimmer));
  });
});

const rxLevelDb = computed(() => {
  const level = runtime.snapshot.rxLevel;
  if (level <= 0) return "-∞ dB";
  const db = 20 * Math.log10(level);
  return `${db.toFixed(1)} dB`;
});

const txLevelDb = computed(() => {
  const level = runtime.snapshot.txLevel;
  if (level <= 0) return "-∞ dB";
  const db = 20 * Math.log10(level);
  return `${db.toFixed(1)} dB`;
});

function describeCallsignRegion(callsign: string) {
  const match = callsign.toUpperCase().match(/[A-Z]+(\d)/);
  if (!match) {
    return t.value.regionUnknown;
  }
  return t.value.zone(match[1]);
}

function normalizeKeyLabel(key: string) {
  if (!key) {
    return "Space";
  }
  if (key === " ") {
    return "Space";
  }
  if (key.startsWith("Key")) {
    return key.slice(3).toUpperCase();
  }
  if (key.startsWith("Digit")) {
    return key.slice(5);
  }
  return key;
}

function clearHoldTimer() {
  if (holdTimerId.value !== null) {
    window.clearTimeout(holdTimerId.value);
    holdTimerId.value = null;
  }
}

function stopAnimationTimer() {
  if (animationTimerId.value !== null) {
    window.clearInterval(animationTimerId.value);
    animationTimerId.value = null;
  }
}

function scheduleHoldActivation() {
  clearHoldTimer();
  holdTimerId.value = window.setTimeout(() => {
    holdTimerId.value = null;
    holdActivated.value = true;
    void runtime.setTx(true);
  }, HOLD_THRESHOLD_MS);
}

async function releasePtt(event?: PointerEvent) {
  flog("[ptt] releasePtt: pressed=", pttPressed.value, "holdActivated=", holdActivated.value, "holdTimer=", holdTimerId.value);
  if (!pttPressed.value) {
    return;
  }
  pttPressed.value = false;
  if (event?.currentTarget instanceof Element) {
    try { event.currentTarget.releasePointerCapture(event.pointerId); } catch { /* ok */ }
  }
  if (holdTimerId.value !== null) {
    clearHoldTimer();
    await runtime.toggleTx();
    return;
  }
  if (holdActivated.value) {
    holdActivated.value = false;
    await runtime.setTx(false);
  }
}

function pressPtt(event?: PointerEvent) {
  flog("[ptt] pressPtt: busy=", runtime.busy, "pressed=", pttPressed.value, "conn=", runtime.snapshot.connection);
  if (runtime.busy || pttPressed.value || runtime.snapshot.connection !== "connected") {
    return;
  }
  // 捕获指针，确保 pointerup 在按钮上触发，即使鼠标移出按钮范围
  if (event?.currentTarget instanceof Element) {
    try { event.currentTarget.setPointerCapture(event.pointerId); } catch { /* ok */ }
  }
  pttPressed.value = true;
  holdActivated.value = false;
  scheduleHoldActivation();
}

function isMatchingPttKey(event: KeyboardEvent) {
  const target = normalizeKeyLabel(runtime.config.pttKey);
  return normalizeKeyLabel(event.code || event.key) === target;
}

function handleGlobalKeydown(event: KeyboardEvent) {
  if (listeningPttKey.value) {
    event.preventDefault();
    pttKeyDraft.value = normalizeKeyLabel(event.code || event.key);
    listeningPttKey.value = false;
    void runtime.saveConfig({
      ...runtime.config,
      pttKey: pttKeyDraft.value,
    });
    return;
  }
  const target = event.target as HTMLElement | null;
  if (target && ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName)) {
    return;
  }
  if (!isMatchingPttKey(event) || event.repeat) {
    return;
  }
  event.preventDefault();
  pressPtt();
}

function handleGlobalKeyup(event: KeyboardEvent) {
  if (!isMatchingPttKey(event)) {
    return;
  }
  event.preventDefault();
  void releasePtt();
}

function beginPttKeyCapture() {
  listeningPttKey.value = true;
}

async function submitMessage() {
  const text = draftMessage.value.trim();
  if (!text) {
    return;
  }
  await runtime.sendMessage(text);
  draftMessage.value = "";
}

async function handleJitterInput(event: Event) {
  const target = event.target as HTMLInputElement | null;
  if (!target) {
    return;
  }
  await runtime.setJitter(Number(target.value));
}

async function saveNetworkConfig() {
  await runtime.saveConfig({
    ...runtime.config,
    pttKey: pttKeyDraft.value,
  });
}

async function toggleFloatingPtt() {
  await togglePttWindow();
}

function toggleLanguage() {
  language.value = language.value === "zh" ? "en" : "zh";
  localStorage.setItem("nrl-pulse-lang", language.value);
}

async function startPttDrag() {
  await startPttWindowDrag();
}

async function closeFloatingWindow() {
  await closePttWindow();
}

async function loginPlatform() {
  loginError.value = "";
  try {
    await platform.login();
    showLogin.value = false;
  } catch (error) {
    loginError.value = error instanceof Error ? error.message : String(error);
  }
}

async function switchGroup(groupId: number) {
  loginError.value = "";
  try {
    await platform.switchGroup(groupId);
  } catch (error) {
    loginError.value = error instanceof Error ? error.message : String(error);
  }
}

function syncConfigDrafts() {
  pttKeyDraft.value = runtime.config.pttKey;
  if (runtime.config.server && platform.servers.some((server) => server.host === runtime.config.server)) {
    platform.selectedServerHost = runtime.config.server;
  }
}

onMounted(async () => {
  try {
    const version = await getVersion();
    const title = `NRL Pulse v${version} © BH4RPN`;
    document.title = title;
    await getCurrentWindow().setTitle(title);
  } catch { /* 权限未授予时不影响后续初始化 */ }
  if (isPttWindow) {
    document.documentElement.classList.add("ptt-window");
    document.body.classList.add("ptt-window");
  }
  await runtime.bootstrap();
  if (!isPttWindow) {
    await platform.bootstrap();
  }
  syncConfigDrafts();
  showLogin.value = !isPttWindow && !platform.loggedIn;
  await onChatMessage((event) => {
    chatMessages.value = [...chatMessages.value, event].slice(-40);
  });
  window.addEventListener("keydown", handleGlobalKeydown);
  window.addEventListener("keyup", handleGlobalKeyup);
  animationTimerId.value = window.setInterval(() => {
    animationTick.value += 1;
  }, 120);
  if (!isPttWindow) {
    void openPttWindow();
    // 启动后静默检查更新
    setTimeout(async () => {
      const info = await checkUpdate();
      if (info.available) updateInfo.value = info;
    }, 3000);
  }
});

async function doUpdate() {
  updateDownloading.value = true;
  updateProgress.value = 0;
  updateTotal.value = 0;
  await downloadAndInstallUpdate((downloaded, total) => {
    updateProgress.value = downloaded;
    updateTotal.value = total ?? 0;
  });
  updateDownloading.value = false;
}

async function manualCheckUpdate() {
  updateInfo.value = null;
  const info = await checkUpdate();
  if (info.available) {
    updateInfo.value = info;
  } else {
    alert(t.value.updateNone);
  }
}

onBeforeUnmount(() => {
  if (isPttWindow) {
    document.documentElement.classList.remove("ptt-window");
    document.body.classList.remove("ptt-window");
  }
  clearHoldTimer();
  stopAnimationTimer();
  window.removeEventListener("keydown", handleGlobalKeydown);
  window.removeEventListener("keyup", handleGlobalKeyup);
});

watch(
  () => runtime.config,
  () => {
    syncConfigDrafts();
  },
  { deep: true },
);

watch(
  () => platform.loggedIn,
  (loggedIn) => {
    showLogin.value = !loggedIn;
  },
  { immediate: true },
);
</script>

<template>
  <main v-if="isPttWindow" class="shell shell-ptt">
    <section class="ptt-float-shell">
      <div class="ptt-float-drag" @pointerdown.prevent="startPttDrag">
        <span class="ptt-float-grip"></span>
      </div>
      <button class="ptt-float-close" :title="t.closePttWindow" @click="closeFloatingWindow">×</button>
      <button
        class="ptt-button ptt-button-floating"
        :class="{
          active: runtime.snapshot.isTransmitting,
          pressed: isPttWindow ? runtime.snapshot.isTransmitting : pttPressed,
          disabled: !!pttStatusReason
        }"
        @pointerdown.prevent="pressPtt($event)"
        @pointerup.prevent="releasePtt($event)"
        @pointercancel.prevent="releasePtt($event)"
      >
        <span class="ptt-ring"></span>
        <span class="ptt-core"></span>
        <span class="ptt-copy">
          <small>{{ pttStatusReason ? pttStatusReason : "PTT" }}</small>
          <strong>{{ txLabel }}</strong>
          <em v-if="!pttStatusReason">{{ pttKeyLabel }}</em>
        </span>
      </button>
    </section>
  </main>

  <main v-else class="shell">
    <header class="topbar">
      <div class="topbar-summary">
        <div class="summary-item summary-callsign">
          <span>{{ t.localCallsign }}</span>
          <strong>{{ platform.loggedIn ? `${runtime.config.callsign}-${runtime.config.ssid}` : "-" }}</strong>
        </div>
        <div class="summary-item">
          <span>{{ t.latency }}</span>
          <strong>{{ platform.loggedIn ? runtime.snapshot.latencyMs : 0 }} ms</strong>
        </div>
        <div class="summary-item">
          <span>{{ t.jitter }}</span>
          <strong>{{ platform.loggedIn ? runtime.snapshot.jitterMs : 0 }} ms</strong>
        </div>
        <div class="summary-item">
          <span>{{ t.loss }}</span>
          <strong>{{ platform.loggedIn ? runtime.snapshot.packetLoss.toFixed(1) : "0.0" }}%</strong>
        </div>
        <div class="summary-item">
          <span>{{ t.queue }}</span>
          <strong>{{ platform.loggedIn ? runtime.snapshot.queuedFrames : 0 }}</strong>
        </div>
        <div class="summary-item summary-signal">
          <div class="signal-stack">
            <div class="signal-row">
              <span>{{ t.receive }}</span>
              <div class="mini-meter vu-meter">
                <span class="mini-meter-fill rx" :style="{ width: platform.loggedIn ? `${runtime.snapshot.rxLevel * 100}%` : '0%' }"></span>
              </div>
              <strong>{{ platform.loggedIn ? rxLevelDb : "-∞ dB" }}</strong>
            </div>
            <div class="signal-row">
              <span>{{ t.transmit }}</span>
              <div class="mini-meter vu-meter">
                <span class="mini-meter-fill tx" :style="{ width: platform.loggedIn ? `${runtime.snapshot.txLevel * 100}%` : '0%' }"></span>
              </div>
              <strong>{{ platform.loggedIn ? txLevelDb : "-∞ dB" }}</strong>
            </div>
          </div>
        </div>
      </div>
      <nav class="topbar-actions">
        <button class="ghost-btn lang-btn" @click="toggleLanguage">
          {{ language === "zh" ? "EN" : t.language }}
        </button>
        <button
          class="ghost-btn"
          :class="{ 'status-connected': platform.loggedIn }"
          :disabled="platform.busy"
          @click="showLogin = !showLogin"
        >
          {{ platform.loggedIn ? t.platformLoggedIn : t.platformLogin }}
        </button>
        <button class="ghost-btn" :disabled="runtime.busy" @click="showLogs = !showLogs">
          {{ showLogs ? t.closeLogs : t.systemLogs }}
        </button>
        <button class="ghost-btn" :disabled="runtime.busy" @click="showSettings = !showSettings">
          {{ showSettings ? t.closeSettings : t.openSettings }}
        </button>
        <button class="ghost-btn" @click="manualCheckUpdate">
          {{ t.checkUpdate }}
        </button>
      </nav>
    </header>

    <!-- 更新提示横幅 -->
    <transition name="drawer-fade">
      <div v-if="updateInfo" class="update-banner">
        <span>{{ t.updateAvailable(updateInfo.version ?? "") }}</span>
        <button
          class="update-banner-btn"
          :disabled="updateDownloading"
          @click="doUpdate"
        >
          {{ updateDownloading
            ? `${t.updateDownloading}${updateTotal ? ' ' + Math.round(updateProgress / updateTotal * 100) + '%' : ''}`
            : t.checkUpdate
          }}
        </button>
        <button class="update-banner-close" @click="updateInfo = null">×</button>
      </div>
    </transition>

    <section class="dashboard-grid">
      <article class="card focus-card">
        <div class="callsign-stage">
          <div class="callsign-tools">
            <button
              class="ghost-btn tool-pill"
              :class="{ 'status-connected': runtime.snapshot.connection === 'connected' }"
              :disabled="runtime.busy"
              @click="
                runtime.snapshot.connection === 'connected'
                  ? runtime.disconnect()
                  : platform.loggedIn
                    ? runtime.connect()
                    : (showLogin = true)
              "
            >
              {{ runtime.snapshot.connection === "connected" ? t.disconnect : t.connect }}
            </button>
            <button
              class="icon-toggle"
              :class="{ active: !runtime.snapshot.isMonitoring }"
              :disabled="runtime.busy"
              :title="runtime.snapshot.isMonitoring ? t.enableMute : t.disableMute"
              @click="runtime.toggleRx()"
            >
              <svg viewBox="0 0 24 24" aria-hidden="true">
                <path
                  v-if="runtime.snapshot.isMonitoring"
                  d="M4 14h3.5l4.5 4V6l-4.5 4H4zm10.8-4.7a4.5 4.5 0 0 1 0 5.4m2.8-8.1a8.2 8.2 0 0 1 0 10.8"
                />
                <path
                  v-else
                  d="M4 14h3.5l4.5 4V6l-4.5 4H4m4.8 2.8 8.8-8.8m0 8.8-8.8-8.8"
                />
              </svg>
            </button>
            <button
              class="icon-toggle record-toggle"
              :class="{ active: runtime.snapshot.recorderEnabled }"
              :disabled="runtime.busy"
              :title="runtime.snapshot.recorderEnabled ? t.stopRecording : t.startRecording"
              @click="runtime.toggleRec()"
            >
              <svg viewBox="0 0 24 24" aria-hidden="true">
                <path
                  v-if="runtime.snapshot.recorderEnabled"
                  d="M12 15.5a3.5 3.5 0 0 0 3.5-3.5V7.5a3.5 3.5 0 1 0-7 0V12a3.5 3.5 0 0 0 3.5 3.5m-6-3.5a6 6 0 0 0 12 0M12 18v3m-3 0h6"
                />
                <path
                  v-else
                  d="M15.5 15.2A3.5 3.5 0 0 1 8.5 12V7.5a3.5 3.5 0 0 1 5.8-2.6M6 12a6 6 0 0 0 9.6 4.8M12 18v3m-3 0h6M4 4l16 16"
                />
              </svg>
            </button>
            <button class="ghost-btn tool-pill" :title="t.pttWindow" @click="toggleFloatingPtt">
              {{ t.pttWindow }}
            </button>
          </div>
          <div class="callsign-spectrum" aria-hidden="true">
            <span
              v-for="(bar, index) in spectrumBars"
              :key="index"
              class="callsign-spectrum-bar"
              :style="{ transform: `scaleY(${bar})` }"
            ></span>
          </div>
          <div class="callsign-display">{{ currentTalker }}</div>
          <div class="callsign-meta">
            <span class="callsign-room callsign-region">{{ currentTalkerRegion }}</span>
            <span class="callsign-room">{{ runtime.config.serverName || "-" }}</span>
            <span class="callsign-room">{{ currentGroupText }}</span>
          </div>
        </div>

        <div class="ops-grid">
          <section class="ops-panel">
            <div class="ops-head">
              <div>
                <p class="section-kicker">{{ t.groupSwitch }}</p>
              </div>
              <div class="ops-head-right">
                <input
                  v-if="platform.loggedIn"
                  v-model="groupSearch"
                  class="group-search"
                  type="text"
                  :placeholder="t.groupSearch"
                />
                <button
                  class="ghost-btn compact-ghost"
                  :disabled="platform.busy || !platform.loggedIn"
                  @click="platform.refreshGroups()"
                >
                  {{ t.refresh }}
                </button>
              </div>
            </div>
            <div v-if="!platform.loggedIn" class="ops-empty">
              <button class="ghost-btn" @click="showLogin = true">{{ t.openLogin }}</button>
            </div>
            <div v-else class="group-stack">
              <button
                v-for="group in filteredGroups"
                :key="group.id"
                class="group-chip"
                :class="{ active: group.id === platform.currentGroupId, 'has-online': (group.onlineDevNumber ?? 0) > 0 }"
                :disabled="platform.busy"
                @click="switchGroup(group.id)"
              >
                <strong>{{ group.id }} · {{ group.name }}</strong>
                <span>{{ group.onlineDevNumber ?? 0 }}/{{ group.totalDevNumber ?? 0 }}</span>
              </button>
            </div>
          </section>

          <section class="ops-panel roster-panel">
            <div class="ops-head">
              <div>
                <p class="section-kicker">{{ t.onlineDevices }}</p>
              </div>
              <div class="roster-stats">
                {{ platform.currentGroup?.onlineDevNumber ?? 0 }}/{{ platform.currentGroup?.totalDevNumber ?? 0 }}
              </div>
            </div>
            <div v-if="!platform.loggedIn" class="ops-empty">
            </div>
            <div v-else-if="platform.onlineDevices.length === 0" class="ops-empty">
              {{ t.noOnlineDevices }}
            </div>
            <div v-else class="roster-list">
              <article v-for="device in platform.onlineDevices" :key="device.id" class="roster-card">
                <div>
                  <strong>{{ device.callsign }}-{{ device.ssid }}</strong>
                  <p>{{ device.name || device.qth || t.onlineDevice }}</p>
                </div>
              </article>
            </div>
          </section>
        </div>
      </article>

      <article class="card chat-card">
        <div class="section-head chat-head">
          <div>
            <p class="section-kicker">{{ t.commandText }}</p>
          </div>
          <span class="chat-status">{{ t.messagesCount(chatMessages.length) }}</span>
        </div>

        <div class="chat-thread">
          <div
            v-for="message in chatMessages"
            :key="message.id"
            class="chat-row"
            :data-side="message.side"
          >
            <div class="chat-bubble" :data-side="message.side">
              <small>{{ message.meta }} · {{ message.time }}</small>
              <p>{{ message.text }}</p>
            </div>
          </div>

        </div>

        <div class="message-box">
          <div class="message-input-wrap">
            <textarea
              v-model="draftMessage"
              rows="4"
              @keydown.ctrl.enter.prevent="submitMessage"
            />
            <button class="primary-btn compact message-send-btn" :disabled="runtime.busy" @click="submitMessage">
              {{ t.sendMessage }}
            </button>
          </div>
        </div>
      </article>
    </section>

    <transition name="drawer-fade">
      <div v-if="showSettings" class="drawer-backdrop" @click="showSettings = false"></div>
    </transition>

    <transition name="drawer-fade">
      <div v-if="showLogs" class="drawer-backdrop" @click="showLogs = false"></div>
    </transition>

    <transition name="drawer-fade">
      <div v-if="showLogin" class="drawer-backdrop" @click="showLogin = false"></div>
    </transition>

    <aside class="settings-drawer" :data-open="showSettings">
      <div class="drawer-head">
        <div>
          <h2>参数</h2>
        </div>
        <button class="ghost-btn compact-ghost" @click="showSettings = false">{{ t.close }}</button>
      </div>

      <div class="settings-list">
        <div class="flag-grid">
          <button class="ghost-btn flag-card keybind-box" :disabled="runtime.busy" @click="beginPttKeyCapture">
            <span>{{ t.pttHotkey }}</span>
            <strong>{{ listeningPttKey ? t.anyKey : normalizeKeyLabel(runtime.config.pttKey) }}</strong>
          </button>
          <div class="flag-card">
            <span>{{ t.agc }}</span>
            <strong>{{ runtime.snapshot.devices.agcEnabled ? t.enabled : t.disabled }}</strong>
          </div>
          <div class="flag-card">
            <span>{{ t.noiseSuppression }}</span>
            <strong>{{ runtime.snapshot.devices.noiseSuppression ? t.enabled : t.disabled }}</strong>
          </div>
          <div
            class="flag-card"
            :class="{ 'flag-card-disabled': !runtime.snapshot.devices.aecEnabled }"
            :title="runtime.snapshot.devices.aecEnabled ? '' : t.aecUnsupported"
          >
            <span>{{ t.aec }}</span>
            <strong>{{ runtime.snapshot.devices.aecEnabled ? t.enabled : t.disabled }}</strong>
          </div>
        </div>
        <div class="setting-row">
          <span>{{ t.inputDevice }}</span>
          <strong>{{ runtime.snapshot.devices.inputDevice }}</strong>
        </div>
        <div class="setting-row">
          <span>{{ t.outputDevice }}</span>
          <strong>{{ runtime.snapshot.devices.outputDevice }}</strong>
        </div>
        <div class="setting-row">
          <span>{{ t.sampleRate }}</span>
          <strong>{{ runtime.snapshot.devices.sampleRate }} Hz</strong>
        </div>
      </div>

      <div class="jitter-editor">
        <div class="jitter-label">
          <span>{{ t.jitterBuffer }}</span>
          <strong>{{ runtime.snapshot.devices.jitterBufferMs }} ms</strong>
        </div>
        <input
          type="range"
          min="40"
          max="300"
          step="10"
          :value="runtime.snapshot.devices.jitterBufferMs"
          @input="handleJitterInput"
        />
      </div>
    </aside>

    <aside class="logs-drawer" :data-open="showLogs">
      <div class="drawer-head">
        <div>
          <h2>日志</h2>
        </div>
        <button class="ghost-btn compact-ghost" @click="showLogs = false">{{ t.close }}</button>
      </div>

      <div class="log-list">
        <div v-if="runtime.timeline.length === 0" class="log-empty">
          {{ t.noLogs }}
        </div>
        <article
          v-for="entry in runtime.timeline"
          :key="entry.id"
          class="log-card"
          :data-tone="entry.tone"
        >
          <div class="log-meta">
            <strong>{{ entry.title }}</strong>
            <span>{{ entry.time }}</span>
          </div>
          <p>{{ entry.detail }}</p>
        </article>
      </div>
    </aside>

    <aside class="auth-drawer" :data-open="showLogin">
      <div class="drawer-head">
        <div>
          <h2>{{ t.serverLogin }}</h2>
        </div>
        <button class="ghost-btn compact-ghost" @click="showLogin = false">{{ t.close }}</button>
      </div>

      <div class="settings-list">
        <div class="setting-form auth-form">
          <div class="login-server-row">
            <label class="login-server">
              <span>{{ t.loginServer }}</span>
              <select v-model="platform.selectedServerHost">
                <option v-for="server in platform.servers" :key="server.host" :value="server.host">
                  {{ server.name }} · {{ server.host }}:{{ server.port }} · {{ server.online }}/{{ server.total }}
                </option>
              </select>
            </label>
            <button class="icon-btn" :disabled="platform.busy" :title="t.refreshServers" @click="platform.refreshServers()">
              <svg viewBox="0 0 24 24" aria-hidden="true">
                <path d="M4 12a8 8 0 0 1 14.9-5.3L21 9v6h-6l2.4-2.4A10 10 0 0 0 4.3 11H2v4h4.3A8 8 0 0 1 4 12z"/>
              </svg>
            </button>
          </div>
          <label class="full-width">
            <span>{{ t.username }}</span>
            <input v-model="platform.username" type="text" autocomplete="username" />
          </label>
          <label class="full-width">
            <span>{{ t.password }}</span>
            <input
              v-model="platform.password"
              type="password"
              autocomplete="current-password"
              @keydown.enter.prevent="loginPlatform"
            />
          </label>
          <div class="login-btn-wrapper">
            <button class="primary-btn" :disabled="platform.busy" @click="loginPlatform">
              {{ platform.busy ? t.loggingIn : platform.loggedIn ? t.relogin : t.loginPlatformAction }}
            </button>
          </div>
        </div>

        <div v-if="loginError" class="auth-error">{{ loginError }}</div>

        <template v-if="platform.loggedIn && platform.user">
          <div class="setting-row">
            <span>{{ t.currentAccount }}</span>
            <strong>{{ platform.user.name || platform.user.callsign }}</strong>
          </div>
          <div class="setting-row">
            <span>{{ t.voiceCallsign }}</span>
            <strong>{{ platform.user.callsign }}-{{ runtime.config.ssid }}</strong>
          </div>
          <div class="setting-row">
            <span>{{ t.currentGroupLabel }}</span>
            <strong>{{ platform.currentGroup?.name || "-" }}</strong>
          </div>
          <button class="ghost-btn" :disabled="platform.busy" @click="platform.logout()">
            {{ t.logoutLocal }}
          </button>
        </template>
      </div>
    </aside>
  </main>
</template>
