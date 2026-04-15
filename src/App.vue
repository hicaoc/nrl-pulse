<script setup lang="ts">
import { computed, markRaw, nextTick, onBeforeUnmount, onMounted, ref, shallowRef, watch } from "vue";
import { getVersion } from "@tauri-apps/api/app";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
import {
  checkUpdate,
  closePttWindow,
  downloadAndInstallUpdate,
  flog,
  getDefaultAudioDir,
  onChatMessage,
  openPttWindow,
  readVoiceFile,
  startPttWindowDrag,
  togglePttWindow,
} from "@/lib/tauri";
import type { UpdateInfo } from "@/lib/tauri";
import { usePlatformStore } from "@/stores/platform";
import { useRuntimeStore } from "@/stores/runtime";
import type { ChatMessageEvent } from "@/types";

type Lang = "zh" | "en";

// roundRect polyfill —— macOS 12 / WKWebView <16 没有该 API，且早期 Safari 16
// 在半径超过短边一半时会抛 RangeError 而不是按 spec 钳制。统一替换成自绘实现。
{
  const proto = CanvasRenderingContext2D.prototype as CanvasRenderingContext2D & {
    roundRect?: (x: number, y: number, w: number, h: number, r?: number | number[]) => void;
  };
  proto.roundRect = function (x, y, w, h, r) {
    let tl = 0, tr = 0, br = 0, bl = 0;
    if (Array.isArray(r)) {
      if (r.length === 1) tl = tr = br = bl = r[0];
      else if (r.length === 2) { tl = br = r[0]; tr = bl = r[1]; }
      else if (r.length === 3) { tl = r[0]; tr = bl = r[1]; br = r[2]; }
      else { tl = r[0]; tr = r[1]; br = r[2]; bl = r[3]; }
    } else if (typeof r === "number") {
      tl = tr = br = bl = r;
    }
    const maxR = Math.min(Math.abs(w), Math.abs(h)) / 2;
    tl = Math.max(0, Math.min(tl, maxR));
    tr = Math.max(0, Math.min(tr, maxR));
    br = Math.max(0, Math.min(br, maxR));
    bl = Math.max(0, Math.min(bl, maxR));
    this.moveTo(x + tl, y);
    this.lineTo(x + w - tr, y);
    this.quadraticCurveTo(x + w, y, x + w, y + tr);
    this.lineTo(x + w, y + h - br);
    this.quadraticCurveTo(x + w, y + h, x + w - br, y + h);
    this.lineTo(x + bl, y + h);
    this.quadraticCurveTo(x, y + h, x, y + h - bl);
    this.lineTo(x, y + tl);
    this.quadraticCurveTo(x, y, x + tl, y);
  };
}

const HOLD_THRESHOLD_MS = 320;
const runtime = useRuntimeStore();
const platform = usePlatformStore();
const isPttWindow = window.location.hash === "#ptt";

const draftMessage = ref("");
const pttKeyDraft = ref("Space");
const voiceSavePathDraft = ref("");
const defaultAudioPath = ref("");
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
const clockTimerId = ref<number | null>(null);
const animationTick = ref(0);
const language = ref<Lang>((localStorage.getItem("nrl-pulse-lang") as Lang) || "zh");
const chatMessages = shallowRef<
  ChatMessageEvent[]
>([]);
const currentTime = ref(new Date());

const playingMessageId = ref<string | null>(null);
let activeVoiceAudio: HTMLAudioElement | null = null;
let activeVoiceUrl: string | null = null;
const waveformCanvases = new Map<string, HTMLCanvasElement>();
const waveformHoverIndex = new Map<string, number | null>();
const rxMeterCanvas = ref<HTMLCanvasElement | null>(null);
const txMeterCanvas = ref<HTMLCanvasElement | null>(null);
const spectrumCanvas = ref<HTMLCanvasElement | null>(null);
const meterDisplayLevel = new Map<"rx" | "tx", number>();
const meterPeakLevel = new Map<"rx" | "tx", number>();
const rxPeakDisplay = ref(0);
const txPeakDisplay = ref(0);
const spectrumHoverIndex = ref<number | null>(null);
const spectrumDisplayLevels = ref<number[]>([]);
const spectrumPeakLevels = ref<number[]>([]);

function normalizeChatMessage(event: ChatMessageEvent): ChatMessageEvent {
  return markRaw({
    ...event,
    waveform: event.waveform ? markRaw(event.waveform) : undefined,
  });
}

function appendChatMessage(event: ChatMessageEvent) {
  chatMessages.value = [...chatMessages.value, normalizeChatMessage(event)].slice(-40);
}

function drawWaveform(messageId: string, waveform: number[] | undefined, isPlaying: boolean) {
  const canvas = waveformCanvases.get(messageId);
  if (!canvas) return;

  const cssWidth = canvas.clientWidth || 140;
  const cssHeight = canvas.clientHeight || 20;
  const dpr = window.devicePixelRatio || 1;
  const width = Math.max(1, Math.round(cssWidth * dpr));
  const height = Math.max(1, Math.round(cssHeight * dpr));
  if (canvas.width !== width || canvas.height !== height) {
    canvas.width = width;
    canvas.height = height;
  }

  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  ctx.clearRect(0, 0, width, height);

  const bars = waveform?.length ? waveform : Array.from({ length: 40 }, () => 0.08);
  const hoverIndex = waveformHoverIndex.get(messageId);
  const gap = Math.max(1 * dpr, Math.floor(width * 0.008));
  const barWidth = Math.max(2 * dpr, (width - gap * (bars.length - 1)) / bars.length);
  let x = 0;

  for (let i = 0; i < bars.length; i++) {
    const level = Math.max(0.12, Math.min(1, bars[i] ?? 0));
    const barHeight = Math.max(4 * dpr, level * height);
    const y = (height - barHeight) / 2;
    const hovered = hoverIndex === i;

    ctx.fillStyle = hovered
      ? "rgba(196, 247, 255, 1)"
      : isPlaying
        ? "rgba(255, 255, 255, 0.92)"
        : "rgba(247, 242, 232, 0.74)";
    if (hovered) {
      ctx.shadowColor = "rgba(91, 192, 255, 0.62)";
      ctx.shadowBlur = 14 * dpr;
    } else {
      ctx.shadowBlur = 0;
    }
    ctx.beginPath();
    ctx.roundRect(x, y, barWidth, barHeight, Math.min(barWidth / 2, 2 * dpr));
    ctx.fill();
    x += barWidth + gap;
  }
  ctx.shadowBlur = 0;
}

function redrawWaveforms() {
  for (const message of chatMessages.value) {
    drawWaveform(message.id, message.waveform, playingMessageId.value === message.id);
  }
}

function setWaveformCanvas(messageId: string, el: HTMLCanvasElement | null) {
  if (el) {
    waveformCanvases.set(messageId, el);
    void nextTick(() => {
      const message = chatMessages.value.find((item) => item.id === messageId);
      drawWaveform(messageId, message?.waveform, playingMessageId.value === messageId);
    });
    return;
  }
  waveformCanvases.delete(messageId);
  waveformHoverIndex.delete(messageId);
}

function handleWaveformHover(messageId: string, event: MouseEvent) {
  const message = chatMessages.value.find((item) => item.id === messageId);
  const bars = message?.waveform;
  if (!bars?.length) return;
  const canvas = waveformCanvases.get(messageId);
  if (!canvas) return;
  const rect = canvas.getBoundingClientRect();
  if (rect.width <= 0) return;
  const index = Math.min(
    bars.length - 1,
    Math.max(0, Math.floor(((event.clientX - rect.left) / rect.width) * bars.length)),
  );
  if (waveformHoverIndex.get(messageId) !== index) {
    waveformHoverIndex.set(messageId, index);
    drawWaveform(messageId, bars, playingMessageId.value === messageId);
  }
}

function clearWaveformHover(messageId: string) {
  if (!waveformHoverIndex.has(messageId) && !waveformCanvases.has(messageId)) return;
  waveformHoverIndex.set(messageId, null);
  const message = chatMessages.value.find((item) => item.id === messageId);
  drawWaveform(messageId, message?.waveform, playingMessageId.value === messageId);
}

function prepareCanvas(canvas: HTMLCanvasElement, fallbackWidth: number, fallbackHeight: number) {
  const cssWidth = canvas.clientWidth || fallbackWidth;
  const cssHeight = canvas.clientHeight || fallbackHeight;
  const dpr = window.devicePixelRatio || 1;
  const width = Math.max(1, Math.round(cssWidth * dpr));
  const height = Math.max(1, Math.round(cssHeight * dpr));
  if (canvas.width !== width || canvas.height !== height) {
    canvas.width = width;
    canvas.height = height;
  }
  const ctx = canvas.getContext("2d");
  if (!ctx) return null;
  return { ctx, width, height, dpr };
}

function drawMeter(canvas: HTMLCanvasElement | null, level: number, tone: "rx" | "tx") {
  if (!canvas) return;
  const prepared = prepareCanvas(canvas, 120, 10);
  if (!prepared) return;
  const { ctx, width, height, dpr } = prepared;
  const previousDisplay = meterDisplayLevel.get(tone) ?? 0;
  const previousPeak = meterPeakLevel.get(tone) ?? 0;
  const displayLevel = level > previousDisplay
    ? level
    : Math.max(level, previousDisplay - 0.014);
  const peakLevel = level >= previousPeak
    ? level
    : Math.max(displayLevel, previousPeak - 0.006);
  meterDisplayLevel.set(tone, displayLevel);
  meterPeakLevel.set(tone, peakLevel);
  if (tone === "rx") {
    rxPeakDisplay.value = peakLevel;
  } else {
    txPeakDisplay.value = peakLevel;
  }
  const peakX = Math.max(0, Math.min(width - 2, width * peakLevel));
  const segmentGap = Math.max(1, Math.round(dpr));
  const segmentCount = 18;
  const segmentWidth = (width - segmentGap * (segmentCount - 1)) / segmentCount;

  ctx.clearRect(0, 0, width, height);
  const bg = ctx.createLinearGradient(0, 0, 0, height);
  bg.addColorStop(0, "rgba(255,255,255,0.05)");
  bg.addColorStop(1, "rgba(255,255,255,0.015)");
  ctx.fillStyle = bg;
  ctx.beginPath();
  ctx.roundRect(0, 0, width, height, 3 * dpr);
  ctx.fill();
  ctx.strokeStyle = "rgba(255,255,255,0.06)";
  ctx.lineWidth = Math.max(1, dpr * 0.8);
  ctx.stroke();

  let x = 0;
  for (let i = 0; i < segmentCount; i++) {
    const segmentStart = x;
    const segmentEnd = x + segmentWidth;
    const threshold = (i + 1) / segmentCount;
    const active = displayLevel >= threshold;
    let color = "rgba(255,255,255,0.08)";
    if (active) {
      if (i < Math.floor(segmentCount * 0.65)) {
        color = tone === "rx" ? "rgba(88, 203, 255, 0.95)" : "rgba(255, 180, 97, 0.95)";
      } else if (i < Math.floor(segmentCount * 0.88)) {
        color = "rgba(255, 211, 106, 0.95)";
      } else {
        color = "rgba(255, 112, 112, 0.98)";
      }
    } else {
      if (i < Math.floor(segmentCount * 0.65)) {
        color = tone === "rx" ? "rgba(88, 203, 255, 0.12)" : "rgba(255, 180, 97, 0.12)";
      } else if (i < Math.floor(segmentCount * 0.88)) {
        color = "rgba(255, 211, 106, 0.11)";
      } else {
        color = "rgba(255, 112, 112, 0.1)";
      }
    }

    ctx.fillStyle = color;
    if (active) {
      ctx.shadowColor = color;
      ctx.shadowBlur = i >= Math.floor(segmentCount * 0.88) ? 8 * dpr : 5 * dpr;
    } else {
      ctx.shadowBlur = 0;
    }
    ctx.beginPath();
    ctx.roundRect(segmentStart, 0, segmentWidth, height, 2 * dpr);
    ctx.fill();
    ctx.shadowBlur = 0;
    x = segmentEnd + segmentGap;
  }

  ctx.strokeStyle = "rgba(255,255,255,0.05)";
  ctx.lineWidth = Math.max(1, dpr * 0.7);
  for (let i = 1; i < 4; i++) {
    const tickX = Math.round((width / 4) * i) + 0.5;
    ctx.beginPath();
    ctx.moveTo(tickX, 1);
    ctx.lineTo(tickX, height - 1);
    ctx.stroke();
  }

  ctx.fillStyle = tone === "rx" ? "rgba(227, 250, 255, 0.98)" : "rgba(255, 241, 207, 0.98)";
  ctx.shadowColor = tone === "rx" ? "rgba(91, 192, 255, 0.5)" : "rgba(255, 145, 87, 0.44)";
  ctx.shadowBlur = 8 * dpr;
  ctx.fillRect(peakX, 0, Math.max(2, 2 * dpr), height);
  ctx.shadowBlur = 0;
}

function drawSpectrumCanvas() {
  if (!spectrumCanvas.value) return;
  const prepared = prepareCanvas(spectrumCanvas.value, 800, 220);
  if (!prepared) return;
  const { ctx, width, height, dpr } = prepared;
  const bars = spectrumBars.value;
  const hoverIndex = spectrumHoverIndex.value;
  const displayLevels = spectrumDisplayLevels.value;
  const peakLevels = spectrumPeakLevels.value;
  if (displayLevels.length !== bars.length) {
    spectrumDisplayLevels.value = Array.from({ length: bars.length }, (_, index) => bars[index] ?? 0);
  }
  if (peakLevels.length !== bars.length) {
    spectrumPeakLevels.value = Array.from({ length: bars.length }, (_, index) => bars[index] ?? 0);
  }
  const gap = Math.max(3 * dpr, Math.floor(width * 0.006));
  const barWidth = Math.max(6 * dpr, (width - gap * (bars.length - 1)) / bars.length);
  const floorY = height - 2 * dpr;

  ctx.clearRect(0, 0, width, height);
  const bg = ctx.createLinearGradient(0, 0, 0, height);
  bg.addColorStop(0, "rgba(8, 18, 28, 0.08)");
  bg.addColorStop(1, "rgba(8, 18, 28, 0.18)");
  ctx.fillStyle = bg;
  ctx.fillRect(0, 0, width, height);

  ctx.strokeStyle = "rgba(173, 218, 240, 0.08)";
  ctx.lineWidth = 1;
  for (let i = 1; i <= 4; i++) {
    const y = Math.round((height / 5) * i) + 0.5;
    ctx.beginPath();
    ctx.moveTo(0, y);
    ctx.lineTo(width, y);
    ctx.stroke();
  }

  ctx.strokeStyle = "rgba(129, 225, 255, 0.22)";
  ctx.lineWidth = Math.max(1, dpr);
  ctx.beginPath();
  ctx.moveTo(0, floorY);
  ctx.lineTo(width, floorY);
  ctx.stroke();

  let x = 0;
  for (let index = 0; index < bars.length; index++) {
    const bar = bars[index] ?? 0;
    const previousDisplay = spectrumDisplayLevels.value[index] ?? 0;
    const previousPeak = spectrumPeakLevels.value[index] ?? 0;
    const scaled = Math.max(0.04, Math.min(1, bar));
    const displayLevel = scaled > previousDisplay
      ? previousDisplay + (scaled - previousDisplay) * 0.48
      : previousDisplay + (scaled - previousDisplay) * 0.16;
    const peakLevel = scaled >= previousPeak
      ? scaled
      : Math.max(displayLevel, previousPeak - 0.012);
    spectrumDisplayLevels.value[index] = displayLevel;
    spectrumPeakLevels.value[index] = peakLevel;

    const barHeight = Math.max(height * 0.1, displayLevel * (height * 0.88));
    const y = floorY - barHeight;
    const hovered = hoverIndex === index;
    const gradient = ctx.createLinearGradient(0, y, 0, height);
    gradient.addColorStop(0, hovered ? "rgba(231, 252, 255, 0.95)" : "rgba(177, 237, 255, 0.34)");
    gradient.addColorStop(0.28, hovered ? "rgba(167, 238, 255, 0.9)" : "rgba(120, 214, 255, 0.48)");
    gradient.addColorStop(1, hovered ? "rgba(67, 179, 255, 0.78)" : "rgba(63, 164, 232, 0.52)");
    ctx.fillStyle = gradient;
    ctx.shadowColor = hovered ? "rgba(143, 231, 255, 0.34)" : "rgba(129, 225, 255, 0.12)";
    ctx.shadowBlur = hovered ? 20 * dpr : 8 * dpr;
    ctx.beginPath();
    ctx.roundRect(x, y, barWidth, barHeight, [barWidth, barWidth, 2 * dpr, 2 * dpr]);
    ctx.fill();

    const peakY = Math.max(2 * dpr, floorY - peakLevel * (height * 0.88));
    ctx.shadowBlur = 0;
    ctx.fillStyle = hovered ? "rgba(248, 254, 255, 0.98)" : "rgba(227, 248, 255, 0.8)";
    ctx.fillRect(x, peakY, barWidth, hovered ? 3 * dpr : 2 * dpr);

    ctx.fillStyle = hovered ? "rgba(255,255,255,0.22)" : "rgba(255,255,255,0.08)";
    ctx.fillRect(x, y, Math.max(1, barWidth * 0.18), barHeight);
    x += barWidth + gap;
  }
  ctx.shadowBlur = 0;
}

function redrawRealtimeCanvases() {
  drawMeter(rxMeterCanvas.value, platform.loggedIn ? runtime.snapshot.rxLevel : 0, "rx");
  drawMeter(txMeterCanvas.value, platform.loggedIn ? runtime.snapshot.txLevel : 0, "tx");
  drawSpectrumCanvas();
}

function handleSpectrumHover(event: MouseEvent) {
  const canvas = spectrumCanvas.value;
  if (!canvas) return;
  const rect = canvas.getBoundingClientRect();
  if (rect.width <= 0 || spectrumBars.value.length === 0) return;
  const index = Math.min(
    spectrumBars.value.length - 1,
    Math.max(0, Math.floor(((event.clientX - rect.left) / rect.width) * spectrumBars.value.length)),
  );
  if (spectrumHoverIndex.value !== index) {
    spectrumHoverIndex.value = index;
    drawSpectrumCanvas();
  }
}

function clearSpectrumHover() {
  if (spectrumHoverIndex.value === null) return;
  spectrumHoverIndex.value = null;
  drawSpectrumCanvas();
}

async function playVoiceMessage(message: ChatMessageEvent) {
  if (!message.filePath) return;

  if (playingMessageId.value === message.id) {
    activeVoiceAudio?.pause();
    activeVoiceAudio = null;
    if (activeVoiceUrl) {
      URL.revokeObjectURL(activeVoiceUrl);
      activeVoiceUrl = null;
    }
    playingMessageId.value = null;
    return;
  }

  if (activeVoiceAudio) {
    activeVoiceAudio.pause();
    activeVoiceAudio = null;
  }
  if (activeVoiceUrl) {
    URL.revokeObjectURL(activeVoiceUrl);
    activeVoiceUrl = null;
  }

  playingMessageId.value = message.id;

  try {
    const bytes = await readVoiceFile(message.filePath);
    const blob = new Blob([new Uint8Array(bytes)], { type: "audio/wav" });
    const objectUrl = URL.createObjectURL(blob);
    const audio = new Audio(objectUrl);
    activeVoiceAudio = audio;
    activeVoiceUrl = objectUrl;

    audio.onended = () => {
      if (activeVoiceAudio === audio) {
        activeVoiceAudio = null;
        if (activeVoiceUrl) {
          URL.revokeObjectURL(activeVoiceUrl);
          activeVoiceUrl = null;
        }
        playingMessageId.value = null;
      }
    };

    await audio.play();
  } catch (e) {
    console.error("Failed to play voice message:", e);
    activeVoiceAudio = null;
    if (activeVoiceUrl) {
      URL.revokeObjectURL(activeVoiceUrl);
      activeVoiceUrl = null;
    }
    playingMessageId.value = null;
  }
}

function isVoiceMessage(message: ChatMessageEvent): boolean {
  return message.type === 'voice';
}

function getVoiceBubbleWidth(duration: number | undefined): number {
  const minWidth = 50;
  const maxWidth = 80;
  if (!duration) return minWidth;
  const estimatedSeconds = duration;
  const width = minWidth + (estimatedSeconds * 4);
  return Math.min(maxWidth, Math.max(minWidth, width));
}

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
    voiceSavePath: "语音保存路径",
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
    updateNow: "立即更新",
    checkUpdate: "更新",
    mute: "静音",
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
    voiceSavePath: "Voice Save Path",
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
    updateNow: "Update Now",
    checkUpdate: "Update",
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
const systemClockText = computed(() =>
  new Intl.DateTimeFormat(language.value === "zh" ? "zh-CN" : "en-US", {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  }).format(currentTime.value),
);
const systemDateText = computed(() =>
  new Intl.DateTimeFormat(language.value === "zh" ? "zh-CN" : "en-US", {
    year: "numeric",
    month: "short",
    day: "numeric",
    weekday: "short",
  }).format(currentTime.value),
);

function formatDb(level: number): string {
  if (level <= 0) return "-∞";
  const db = 20 * Math.log10(level);
  return `${db.toFixed(1)}`;
}

function formatDualDb(primary: number, peak: number): string {
  const primaryText = formatDb(primary);
  const peakText = formatDb(peak);
  if (primaryText === "-∞" && peakText === "-∞") {
    return "-∞ dB";
  }
  return `${primaryText} · ${peakText} dB`;
}

const rxLevelDb = computed(() => formatDualDb(runtime.snapshot.rxLevel, rxPeakDisplay.value));

const txLevelDb = computed(() => formatDualDb(runtime.snapshot.txLevel, txPeakDisplay.value));

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

function stopClockTimer() {
  if (clockTimerId.value !== null) {
    window.clearInterval(clockTimerId.value);
    clockTimerId.value = null;
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
    voiceSavePath: voiceSavePathDraft.value,
  });
}

async function toggleFloatingPtt() {
  await togglePttWindow();
}

async function browseVoicePath() {
  const selected = await open({
    directory: true,
    multiple: false,
    title: language.value === "zh" ? "选择语音保存路径" : "Select Voice Save Path",
  });
  if (selected && typeof selected === "string") {
    voiceSavePathDraft.value = selected;
    await saveNetworkConfig();
  }
}

function toggleLanguage() {
  language.value = language.value === "zh" ? "en" : "zh";
  localStorage.setItem("nrl-pulse-lang", language.value);
}

async function onHeaderPointerDown(event: PointerEvent) {
  const target = event.target as HTMLElement | null;
  if (target?.closest(".ptt-console-close")) {
    return;
  }
  event.preventDefault();
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
  voiceSavePathDraft.value = runtime.config.voiceSavePath || "";
  if (runtime.config.server && platform.servers.some((server) => server.host === runtime.config.server)) {
    platform.selectedServerHost = runtime.config.server;
  }
}

onMounted(async () => {
  try {
    const version = await getVersion();
    const title = `NRL Pulse v${version} © BH4RPN 2026 , BA4RN BG6FCS BH4TDV BD4RFG BD4VKI BI4UMD BA4QAO ...  `;
    document.title = title;
    await getCurrentWindow().setTitle(title);
  } catch { /* 权限未授予时不影响后续初始化 */ }
  if (isPttWindow) {
    document.documentElement.classList.add("ptt-window");
    document.body.classList.add("ptt-window");
  }
  await runtime.bootstrap();
  defaultAudioPath.value = await getDefaultAudioDir();
  if (!isPttWindow) {
    await platform.bootstrap();
  }
  syncConfigDrafts();
  showLogin.value = !isPttWindow && !platform.loggedIn;
  await onChatMessage((event) => {
    appendChatMessage(event);
  });
  window.addEventListener("keydown", handleGlobalKeydown);
  window.addEventListener("keyup", handleGlobalKeyup);
  animationTimerId.value = window.setInterval(() => {
    animationTick.value += 1;
  }, 120);
  clockTimerId.value = window.setInterval(() => {
    currentTime.value = new Date();
  }, 1000);
  window.addEventListener("resize", redrawWaveforms);
  window.addEventListener("resize", redrawRealtimeCanvases);
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
  try {
    await downloadAndInstallUpdate((downloaded, total) => {
      updateProgress.value = downloaded;
      updateTotal.value = total ?? 0;
    });
  } catch (err) {
    // 下载失败（404、签名校验失败、网络中断等）必须复位 UI，
    // 否则横幅会永远停在"下载中..."。
    flog("[update] download/install failed:", String(err));
    alert(`更新失败: ${String(err)}`);
  } finally {
    updateDownloading.value = false;
  }
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
  activeVoiceAudio?.pause();
  activeVoiceAudio = null;
  if (activeVoiceUrl) {
    URL.revokeObjectURL(activeVoiceUrl);
    activeVoiceUrl = null;
  }
  if (isPttWindow) {
    document.documentElement.classList.remove("ptt-window");
    document.body.classList.remove("ptt-window");
  }
  clearHoldTimer();
  stopAnimationTimer();
  stopClockTimer();
  window.removeEventListener("keydown", handleGlobalKeydown);
  window.removeEventListener("keyup", handleGlobalKeyup);
  window.removeEventListener("resize", redrawWaveforms);
  window.removeEventListener("resize", redrawRealtimeCanvases);
  waveformCanvases.clear();
  waveformHoverIndex.clear();
});

watch(chatMessages, () => {
  void nextTick(redrawWaveforms);
});

watch(playingMessageId, () => {
  void nextTick(redrawWaveforms);
});

watch(
  [
    () => platform.loggedIn,
    () => runtime.snapshot.rxLevel,
    () => runtime.snapshot.txLevel,
    () => runtime.snapshot.isTransmitting,
    () => animationTick.value,
    spectrumBars,
  ],
  () => {
    void nextTick(redrawRealtimeCanvases);
  },
  { immediate: true },
);

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
    <section class="ptt-console" :class="{ 'is-tx': runtime.snapshot.isTransmitting, 'is-offline': !!pttStatusReason }">
      <header class="ptt-console-head" @pointerdown="onHeaderPointerDown">
        <span class="ptt-status-led" :data-state="runtime.snapshot.connection"></span>
        <span class="ptt-status-text">{{ pttStatusReason || connectionLabel }}</span>
        <span class="ptt-grip" aria-hidden="true"></span>
        <button
          class="ptt-console-close"
          :title="t.closePttWindow"
          @pointerdown.stop
          @click.stop="closeFloatingWindow"
        >×</button>
      </header>

      <div class="ptt-console-body">
        <button
          class="ptt-dial"
          :class="{
            active: runtime.snapshot.isTransmitting,
            pressed: runtime.snapshot.isTransmitting,
            disabled: !!pttStatusReason,
          }"
          @pointerdown.prevent="pressPtt($event)"
          @pointerup.prevent="releasePtt($event)"
          @pointercancel.prevent="releasePtt($event)"
        >
          <span class="ptt-dial-halo"></span>
          <span class="ptt-dial-ring"></span>
          <span class="ptt-dial-core">
            <span class="ptt-dial-label">PTT</span>
            <span class="ptt-dial-sub">{{ pttKeyLabel }}</span>
          </span>
        </button>

        <div class="ptt-console-info">
          <span class="ptt-info-kicker ptt-info-key">⌨ {{ pttKeyLabel }}</span>
          <strong class="ptt-info-callsign">{{ txLabel }}</strong>
          <div class="ptt-info-meta">
            <span class="ptt-info-chip ptt-info-room" :title="runtime.snapshot.roomName || '—'">
              {{ runtime.snapshot.roomName || "—" }}
            </span>
          </div>
        </div>
      </div>
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
      </div>
      <div class="summary-item summary-signal">
        <div class="signal-stack">
          <div class="signal-row">
            <span>{{ t.receive }}</span>
            <div class="mini-meter vu-meter">
              <canvas ref="rxMeterCanvas" class="mini-meter-canvas" width="120" height="10"></canvas>
            </div>
            <strong>{{ platform.loggedIn ? rxLevelDb : "-∞ dB" }}</strong>
          </div>
          <div class="signal-row">
            <span>{{ t.transmit }}</span>
            <div class="mini-meter vu-meter">
              <canvas ref="txMeterCanvas" class="mini-meter-canvas" width="120" height="10"></canvas>
            </div>
            <strong>{{ platform.loggedIn ? txLevelDb : "-∞ dB" }}</strong>
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
        <span class="update-banner-msg">
          {{ updateDownloading ? t.updateDownloading : t.updateAvailable(updateInfo.version ?? "") }}
        </span>
        <template v-if="updateDownloading">
          <div class="update-progress-wrap">
            <div class="update-progress-bar" :class="{ indeterminate: !updateTotal }">
              <div
                class="update-progress-fill"
                :style="{ width: updateTotal ? Math.round(updateProgress / updateTotal * 100) + '%' : '100%' }"
              ></div>
            </div>
            <span class="update-progress-pct">
              {{ updateTotal ? Math.round(updateProgress / updateTotal * 100) + '%' : '...' }}
            </span>
          </div>
        </template>
        <template v-else>
          <button class="update-banner-btn" @click="doUpdate">
            {{ t.updateNow }}
          </button>
          <button class="update-banner-close" @click="updateInfo = null">×</button>
        </template>
      </div>
    </transition>

    <section class="dashboard-grid">
      <article class="card focus-card">
        <div class="callsign-stage">
          <div class="system-clock" aria-label="System time">
            <strong class="system-clock-time">{{ systemDateText }} · {{ systemClockText }}</strong>
          </div>
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
            <button class="ghost-btn tool-pill" :title="t.pttWindow" @click="toggleFloatingPtt">
              {{ t.pttWindow }}
            </button>
          </div>
          <div class="callsign-spectrum" aria-hidden="true">
            <canvas
              ref="spectrumCanvas"
              class="callsign-spectrum-canvas"
              width="960"
              height="240"
              @mousemove="handleSpectrumHover"
              @mouseleave="clearSpectrumHover"
            ></canvas>
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
            <div
              class="chat-bubble"
              :class="{ 'voice-bubble': isVoiceMessage(message), 'playing': playingMessageId === message.id }"
              :data-side="message.side"
              :style="isVoiceMessage(message) ? { width: getVoiceBubbleWidth(message.duration) + '%' } : {}"
              @click="isVoiceMessage(message) && playVoiceMessage(message)"
            >
              <small>{{ message.meta }} · {{ message.time }}</small>
              <template v-if="isVoiceMessage(message)">
                <div class="voice-content">
                  <div class="voice-icon" :class="{ playing: playingMessageId === message.id }">
                    <svg viewBox="0 0 24 24" width="16" height="16" fill="currentColor">
                      <path v-if="playingMessageId !== message.id" d="M8 5v14l11-7z"/>
                      <path v-else d="M6 19h4V5H6v14zm8-14v14h4V5h-4z"/>
                    </svg>
                  </div>
                  <canvas
                    :ref="(el) => setWaveformCanvas(message.id, el as HTMLCanvasElement | null)"
                    class="voice-waveform-canvas"
                    width="160"
                    height="20"
                    @mousemove="handleWaveformHover(message.id, $event)"
                    @mouseleave="clearWaveformHover(message.id)"
                  />
                </div>
              </template>
              <template v-else>
                <p>{{ message.text }}</p>
              </template>
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
        <div class="setting-row voice-path-row">
          <span>{{ t.voiceSavePath }}</span>
          <div class="voice-path-input-row">
            <input
              v-model="voiceSavePathDraft"
              type="text"
              class="text-input"
              :placeholder="language === 'zh' ? '留空使用默认路径' : 'Empty for default'"
              @blur="saveNetworkConfig"
            />
            <button class="ghost-btn compact" @click="browseVoicePath">
              {{ language === 'zh' ? '浏览' : 'Browse' }}
            </button>
          </div>
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
