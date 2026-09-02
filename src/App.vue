<script setup lang="ts">
import { computed, markRaw, nextTick, onBeforeUnmount, onMounted, ref, shallowRef, watch } from "vue";
import { getVersion } from "@tauri-apps/api/app";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";
import { platformFetchGroupDevices, platformRegister } from "@/lib/platform";
import MonitorWindow from "@/components/MonitorWindow.vue";
import { useFmoStore } from "@/stores/fmo";
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
  openMonitorWindow,
} from "@/lib/tauri";
import type { UpdateInfo } from "@/lib/tauri";
import { usePlatformStore } from "@/stores/platform";
import { useRuntimeStore } from "@/stores/runtime";
import type { ChatMessageEvent, FmoBeaconConfig, FmoBroadcastConfig, FmoClient, FmoServer, PlatformDevice, PlatformGroup, PlatformRegisterPayload, PlatformServer, SerialTunnelConfig, TimelineEvent } from "@/types";

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
const fmo = useFmoStore();
const isPttWindow = window.location.hash === "#ptt";
const isMonitorWindow = window.location.hash === "#monitor";

const protocol = computed(() => runtime.config.protocol || "nrl");
const isFmo = computed(() => protocol.value === "fmo");
// 顶部仪表/通话标识激活条件：NRL 需平台登录，FMO 只要协议激活即可
const uiActive = computed(() => isFmo.value || runtime.snapshot.connection === "connected");

const draftMessage = ref("");
const pttKeyDraft = ref("Space");
const voiceSavePathDraft = ref("");
const serialTunnelDraft = ref<SerialTunnelConfig>({
  mode: "physical",
  autoStart: false,
  portName: "",
  baudRate: 115200,
  dataBits: 8,
  parity: "none",
  stopBits: "one",
  flowControl: "none",
});
const defaultAudioPath = ref("");
const showSettings = ref(false);
const settingsTab = ref<"nrl" | "fmo">("nrl");
const logListEl = ref<HTMLElement | null>(null);
// 用户上翻查看历史时暂停自动跟随，回到底部后恢复
const logFollowBottom = ref(true);
// 消息 / 日志 Tab 切换
const chatTab = ref<"messages" | "logs" | "servers" | "users" | "qso">("messages");
const updateInfo = ref<UpdateInfo | null>(null);
const updateDownloading = ref(false);
const updateProgress = ref(0);
const updateTotal = ref(0);
const showLogin = ref(false);
const showRegister = ref(false);
const showTokenLogin = ref(true);
const loginError = ref("");
const registerError = ref("");
const registerSuccess = ref("");
const registerBusy = ref(false);
const listeningPttKey = ref(false);
const pttPressed = ref(false);
const holdActivated = ref(false);
const holdTimerId = ref<number | null>(null);
const realtimeRafId = ref<number | null>(null);
const clockTimerId = ref<number | null>(null);
const serialRateTimerId = ref<number | null>(null);
const animationTick = ref(0);
const serialRxBps = ref(0);
const serialTxBps = ref(0);
const serialRxActive = ref(false);
const serialTxActive = ref(false);
const language = ref<Lang>((localStorage.getItem("nrl-pulse-lang") as Lang) || "zh");
const chatMessages = shallowRef<
  ChatMessageEvent[]
>([]);
const currentTime = ref(new Date());
const registerForm = ref<PlatformRegisterPayload>({
  callsign: "",
  name: "",
  phone: "",
  password: "",
  address: "",
  mail: "",
});
const nrlServerSearch = ref("");
const customNrlServerHost = ref("");
const customNrlServerPort = ref("60050");
const nrlServerError = ref("");
const serverListTab = ref<"nrl" | "fmo">("nrl");
const NRL_FAVORITES_STORAGE_KEY = "nrl-pulse-nrl-favorites";
const nrlFavorites = ref<PlatformServer[]>(loadNrlFavorites());
const registerLicense = ref<{
  name: string;
  size: number;
  bytes: Uint8Array;
} | null>(null);

const fmoAprsCallsign = ref("");
const fmoCertMsg = ref("");
const fmoMuted = ref(false);

// FMO 设备激活（绑定 MAC 自动获取证书）
const fmoActivateServer = ref("");
const fmoActivateMac = ref("");
const fmoActivateMsg = ref("");
const fmoActivating = ref(false);

// FMO 证书：需要完整 4 个
const fmoCertSlots = [
  { name: "cert_user", label: "用户证书 User Cert", file: "cert_user.json" },
  { name: "cert_int", label: "中级证书 Inter CA", file: "cert_int.json" },
  { name: "cert_root", label: "根证书 Root CA", file: "cert_root.json" },
  { name: "cert_devicekey", label: "设备密钥 Device Key", file: "cert_devicekey.json" },
] as const;

const fmoCertReadyCount = computed(
  () => fmoCertSlots.filter((s) => fmo.state.certs.some((c) => c.name === s.name)).length,
);

const MAX_REGISTER_IMAGE_BYTES = 512 * 1024;

const playingMessageId = ref<string | null>(null);
let activeVoiceAudio: HTMLAudioElement | null = null;
let activeVoiceUrl: string | null = null;
const waveformCanvases = new Map<string, HTMLCanvasElement>();
const waveformHoverIndex = new Map<string, number | null>();
const rxMeterCanvas = ref<HTMLCanvasElement | null>(null);
const txMeterCanvas = ref<HTMLCanvasElement | null>(null);
const fmoRxMeterCanvas = ref<HTMLCanvasElement | null>(null);
const fmoTxMeterCanvas = ref<HTMLCanvasElement | null>(null);
const nrlSpectrumCanvas = ref<HTMLCanvasElement | null>(null);
const fmoSpectrumCanvas = ref<HTMLCanvasElement | null>(null);
const spectrumCanvas = ref<HTMLCanvasElement | null>(null);
const meterDisplayLevel = new Map<"rx" | "tx" | "fmo-rx" | "fmo-tx", number>();
const meterPeakLevel = new Map<"rx" | "tx" | "fmo-rx" | "fmo-tx", number>();
// 小频谱柱的逐帧平滑显示值（与电平表同思路：上升快、回落慢），按 nrl/fmo 分开缓存
const miniSpectrumLevels: Record<"nrl" | "fmo", number[]> = { nrl: [], fmo: [] };
const rxPeakDisplay = ref(0);
const txPeakDisplay = ref(0);
const fmoRxPeakDisplay = ref(0);
const fmoTxPeakDisplay = ref(0);
const spectrumHoverIndex = ref<number | null>(null);
let serialRateLast = { rxBytes: 0, txBytes: 0, at: 0 };
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

function drawMeter(
  canvas: HTMLCanvasElement | null,
  level: number,
  tone: "rx" | "tx" | "fmo-rx" | "fmo-tx",
) {
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
  } else if (tone === "tx") {
    txPeakDisplay.value = peakLevel;
  } else if (tone === "fmo-rx") {
    fmoRxPeakDisplay.value = peakLevel;
  } else if (tone === "fmo-tx") {
    fmoTxPeakDisplay.value = peakLevel;
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
  drawMeter(rxMeterCanvas.value, uiActive.value ? runtime.snapshot.rxLevel : 0, "rx");
  drawMeter(txMeterCanvas.value, uiActive.value ? runtime.snapshot.txLevel : 0, "tx");
  drawMeter(fmoRxMeterCanvas.value, fmo.stats.rxLevel, "fmo-rx");
  drawMeter(fmoTxMeterCanvas.value, fmo.stats.txLevel, "fmo-tx");
  drawMiniSpectrum(nrlSpectrumCanvas.value, nrlSpectrumBars.value, "nrl");
  drawMiniSpectrum(fmoSpectrumCanvas.value, fmoSpectrumBars.value, "fmo");
}

// 窄柱频谱：NRL / FMO 各自 box 内的独立波形（柱宽小、更密）
function drawMiniSpectrum(
  canvas: HTMLCanvasElement | null,
  bars: number[],
  tone: "nrl" | "fmo",
) {
  if (!canvas) return;
  const cssWidth = canvas.clientWidth || 460;
  const cssHeight = canvas.clientHeight || 64;
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

  // 窄柱：细柱 + 小间隙，约 48 柱
  const count = 48;
  const gap = Math.max(1 * dpr, Math.round(width * 0.002));
  const barWidth = Math.max(1.5 * dpr, (width - gap * (count - 1)) / count);
  const floorY = height - 3 * dpr;
  const color =
    tone === "nrl"
      ? { c1: "rgba(255,145,87,0.75)", c2: "rgba(255,190,120,0.9)" }
      : { c1: "rgba(91,192,255,0.75)", c2: "rgba(150,225,255,0.9)" };

  const levels = miniSpectrumLevels[tone];
  for (let i = 0; i < count; i++) {
    // 把 28 频段频谱线性映射到 48 窄柱
    const sourceIndex = Math.min(bars.length - 1, Math.floor((i / count) * bars.length));
    const base = bars[sourceIndex] ?? 0;
    // 逐帧平滑：数据低频突发到达（FMO 约 4 次/秒）时柱子也连续起伏，而不是跳变
    const target = Math.min(1, Math.max(0.05, base));
    const prev = levels[i] ?? target;
    const smoothed = target > prev
      ? prev + (target - prev) * 0.5
      : prev + (target - prev) * 0.18;
    levels[i] = smoothed;
    const shimmer = (Math.sin(animationTick.value * 0.025 + i * 0.6) + 1) * 0.02;
    const level = Math.min(1, Math.max(0.05, smoothed + shimmer));
    const barHeight = Math.max(3 * dpr, level * (height - 6 * dpr));
    const y = floorY - barHeight;
    const grad = ctx.createLinearGradient(0, y, 0, height);
    grad.addColorStop(0, color.c2);
    grad.addColorStop(1, color.c1);
    ctx.fillStyle = grad;
    ctx.beginPath();
    ctx.roundRect(i * (barWidth + gap), y, barWidth, barHeight, Math.min(barWidth / 2, 1.5 * dpr));
    ctx.fill();
  }
  ctx.strokeStyle = "rgba(255,255,255,0.06)";
  ctx.lineWidth = Math.max(1, dpr * 0.6);
  ctx.beginPath();
  ctx.moveTo(0, floorY + 0.5);
  ctx.lineTo(width, floorY + 0.5);
  ctx.stroke();
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

function formatBytes(size: number): string {
  if (size < 1024) return `${size} B`;
  if (size < 1024 * 1024) return `${(size / 1024).toFixed(1)} KB`;
  return `${(size / (1024 * 1024)).toFixed(2)} MB`;
}

function resetRegisterState() {
  registerError.value = "";
  registerSuccess.value = "";
}

function resetRegisterForm() {
  registerForm.value = {
    callsign: "",
    name: "",
    phone: "",
    password: "",
    address: "",
    mail: "",
  };
  registerLicense.value = null;
}

function openRegisterForm() {
  loginError.value = "";
  showTokenLogin.value = false;
  resetRegisterState();
  showRegister.value = true;
}

function openTokenLoginForm() {
  loginError.value = "";
  registerError.value = "";
  resetRegisterState();
  showRegister.value = false;
  showTokenLogin.value = true;
}

function backToLoginForm() {
  registerError.value = "";
  showRegister.value = false;
  showTokenLogin.value = false;
}

function fileToObjectUrl(file: Blob): string {
  return URL.createObjectURL(file);
}

function loadImageFromUrl(url: string): Promise<HTMLImageElement> {
  return new Promise((resolve, reject) => {
    const image = new Image();
    image.onload = () => resolve(image);
    image.onerror = () => reject(new Error(t.value.imageReadFailed));
    image.src = url;
  });
}

function canvasToBlob(canvas: HTMLCanvasElement, type: string, quality?: number): Promise<Blob> {
  return new Promise((resolve, reject) => {
    canvas.toBlob((blob) => {
      if (!blob) {
        reject(new Error(t.value.imageReadFailed));
        return;
      }
      resolve(blob);
    }, type, quality);
  });
}

async function compressImageToLimit(file: File) {
  if (file.size <= MAX_REGISTER_IMAGE_BYTES) {
    return {
      name: file.name,
      size: file.size,
      bytes: new Uint8Array(await file.arrayBuffer()),
    };
  }

  const objectUrl = fileToObjectUrl(file);
  try {
    const image = await loadImageFromUrl(objectUrl);
    const canvas = document.createElement("canvas");
    const ctx = canvas.getContext("2d");
    if (!ctx) {
      throw new Error(t.value.imageReadFailed);
    }

    const scaleSteps = [1, 0.9, 0.8, 0.7, 0.6];
    const qualitySteps = [0.86, 0.72, 0.58, 0.46];

    for (const scale of scaleSteps) {
      const width = Math.max(1, Math.round(image.naturalWidth * scale));
      const height = Math.max(1, Math.round(image.naturalHeight * scale));
      canvas.width = width;
      canvas.height = height;
      ctx.clearRect(0, 0, width, height);
      ctx.fillStyle = "#ffffff";
      ctx.fillRect(0, 0, width, height);
      ctx.drawImage(image, 0, 0, width, height);

      for (const quality of qualitySteps) {
        const blob = await canvasToBlob(canvas, "image/jpeg", quality);
        if (blob.size <= MAX_REGISTER_IMAGE_BYTES) {
          const baseName = file.name.replace(/\.[^.]+$/, "") || "license";
          return {
            name: `${baseName}.jpg`,
            size: blob.size,
            bytes: new Uint8Array(await blob.arrayBuffer()),
          };
        }
      }
    }
  } finally {
    URL.revokeObjectURL(objectUrl);
  }

  throw new Error(t.value.imageTooLarge);
}

async function onRegisterImageChange(event: Event) {
  const input = event.target as HTMLInputElement | null;
  const file = input?.files?.[0];
  if (!file) {
    return;
  }
  registerError.value = "";
  try {
    registerLicense.value = await compressImageToLimit(file);
  } catch (error) {
    registerLicense.value = null;
    registerError.value = error instanceof Error ? error.message : String(error);
  } finally {
    if (input) {
      input.value = "";
    }
  }
}

function normalizeHost(value: string): string {
  const trimmed = value.trim().replace(/\/+$/, "");
  if (!trimmed) return "";
  try {
    if (trimmed.startsWith("http://") || trimmed.startsWith("https://")) {
      return new URL(trimmed).hostname;
    }
  } catch {
    // invalid URL treated as plain host
  }
  return trimmed;
}

function nrlFavoriteKey(server: PlatformServer): string {
  return `${normalizeHost(server.host)}:${server.port || 60050}`;
}

function loadNrlFavorites(): PlatformServer[] {
  try {
    const raw = localStorage.getItem(NRL_FAVORITES_STORAGE_KEY);
    const parsed = raw ? JSON.parse(raw) : [];
    return Array.isArray(parsed) ? parsed : [];
  } catch {
    return [];
  }
}

function saveNrlFavorites() {
  localStorage.setItem(NRL_FAVORITES_STORAGE_KEY, JSON.stringify(nrlFavorites.value));
}

function isNrlFavorite(server: PlatformServer): boolean {
  const key = nrlFavoriteKey(server);
  return nrlFavorites.value.some((item) => nrlFavoriteKey(item) === key);
}

function toggleNrlFavorite(server: PlatformServer) {
  const key = nrlFavoriteKey(server);
  const index = nrlFavorites.value.findIndex((item) => nrlFavoriteKey(item) === key);
  if (index >= 0) {
    nrlFavorites.value.splice(index, 1);
  } else {
    nrlFavorites.value.push({ ...server });
  }
  saveNrlFavorites();
}

function resolveAuthHost(): string {
  return platform.resolveAuthServer()?.host?.trim() || "";
}

async function selectNrlServer(server: PlatformServer) {
  nrlServerError.value = "";
  try {
    await platform.selectVoiceServer(server);
  } catch (error) {
    nrlServerError.value = error instanceof Error ? error.message : String(error);
  }
}

async function applyCustomNrlServer() {
  nrlServerError.value = "";
  try {
    await platform.selectCustomVoiceServer(customNrlServerHost.value, customNrlServerPort.value);
  } catch (error) {
    nrlServerError.value = error instanceof Error ? error.message : String(error);
  }
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
    bridgeOff: "互转关闭",
    bridgeTitle: "语音互转：点击循环 关闭 → FMO→NRL → NRL→FMO → 双向",
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
    serialTunnel: "串口透传",
    serialData: "串口数据",
    serialRx: "RX",
    serialTx: "TX",
    serialModePhysical: "物理串口",
    serialPort: "选择串口",
    noSerialPorts: "未发现串口",
    baudRate: "波特率",
    dataBits: "数据位",
    parity: "校验",
    stopBits: "停止位",
    flowControl: "流控",
    serialStats: "收发",
    startSerial: "启动",
    stopSerial: "关闭串口",
    serialUnsupported: "当前系统不支持物理串口桥接",
    parityNone: "无",
    parityOdd: "奇",
    parityEven: "偶",
    stopOne: "1 位",
    stopTwo: "2 位",
    flowNone: "无",
    flowSoftware: "软件",
    flowHardware: "硬件",
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
    authServer: "管理服务器",
    authServerCustom: "自定义管理服务器",
    nrlServerPanel: "NRL服务器",
    fmoServerPanel: "FMO服务器",
    voicePort: "语音端口",
    username: "用户名",
    password: "密码",
    loggingIn: "登录中...",
    relogin: "重新登录",
    loginPlatformAction: "登录平台",
    tokenLoginAction: "Token",
    hamidToken: "HAM ID Token",
    hamidTokenPlaceholder: "hamid_pat_...",
    hamidTokenTip: "粘贴 HAM ID 平台签发的长期 API Token，服务器仍只连接当前选择的 NRL。",
    enterHamidToken: "请输入 HAM ID 长期 Token",
    invalidHamidToken: "Token 必须以 hamid_pat_ 开头",
    refreshServers: "刷新服务器列表",
    currentAccount: "当前账号",
    currentGroupLabel: "当前组",
    logoutLocal: "退出本地登录态",
    openRegister: "注册账号",
    backToLogin: "返回登录",
    registerAction: "提交注册",
    registering: "注册中...",
    serverModeList: "服务器列表",
    serverModeCustom: "自定义服务器",
    customServer: "自定义服务器地址",
    customServerPlaceholder: "例如 m.nrlptt.com",
    callsignField: "呼号",
    realName: "姓名",
    phoneField: "手机号",
    emailField: "邮箱",
    addressField: "地址",
    licenseUpload: "操作证和电台执照合影",
    registerPhotoHint: "超出 512KB 时会自动压缩后上传。",
    registerPendingHint: "注册成功后需等待管理员审核。",
    enterLoginServer: "请输入登录服务器",
    invalidCallsign: "呼号只能包含 5-6 位大写字母和数字",
    enterName: "请输入姓名",
    invalidPhone: "请输入 11 位以上数字的手机号",
    enterPassword: "请输入密码",
    enterAddress: "请输入地址",
    invalidEmail: "请输入有效的邮箱地址",
    uploadLicense: "请上传操作证和电台执照",
    imageTooLarge: "图片过大，请换一张更小的照片",
    imageReadFailed: "图片处理失败，请重试",
    registerFailed: "注册失败，请稍后重试",
    registerSuccess: "注册成功，请等待管理员审核。",
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
    bridgeOff: "Bridge Off",
    bridgeTitle: "Voice bridge: click to cycle Off → FMO→NRL → NRL→FMO → Both",
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
    serialTunnel: "Serial Tunnel",
    serialData: "Serial Data",
    serialRx: "RX",
    serialTx: "TX",
    serialModePhysical: "Physical Port",
    serialPort: "Port",
    noSerialPorts: "No Serial Ports",
    baudRate: "Baud Rate",
    dataBits: "Data Bits",
    parity: "Parity",
    stopBits: "Stop Bits",
    flowControl: "Flow Control",
    serialStats: "RX/TX",
    startSerial: "Start",
    stopSerial: "Stop Serial",
    serialUnsupported: "Physical serial bridge is not supported on this system",
    parityNone: "None",
    parityOdd: "Odd",
    parityEven: "Even",
    stopOne: "1 bit",
    stopTwo: "2 bits",
    flowNone: "None",
    flowSoftware: "Software",
    flowHardware: "Hardware",
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
    loginServer: "Login Server",
    authServer: "Management Server",
    authServerCustom: "Custom management server",
    nrlServerPanel: "NRL Servers",
    fmoServerPanel: "FMO Servers",
    voicePort: "Voice Port",
    username: "Username",
    password: "Password",
    loggingIn: "Signing In...",
    relogin: "Sign In Again",
    loginPlatformAction: "Sign In",
    tokenLoginAction: "Token",
    hamidToken: "HAM ID Token",
    hamidTokenPlaceholder: "hamid_pat_...",
    hamidTokenTip: "Paste a long-lived HAM ID API token. The voice server is still the selected NRL server.",
    enterHamidToken: "Enter your HAM ID token",
    invalidHamidToken: "Token must start with hamid_pat_",
    refreshServers: "Refresh Servers",
    currentAccount: "Account",
    currentGroupLabel: "Current Group",
    logoutLocal: "Clear Local Session",
    openRegister: "Create Account",
    backToLogin: "Back to Login",
    registerAction: "Submit Registration",
    registering: "Registering...",
    serverModeList: "Server List",
    serverModeCustom: "Custom Server",
    customServer: "Custom Server Host",
    customServerPlaceholder: "For example: m.nrlptt.com",
    callsignField: "Callsign",
    realName: "Name",
    phoneField: "Phone",
    emailField: "Email",
    addressField: "Address",
    licenseUpload: "License Photo",
    registerPhotoHint: "Images over 512KB are compressed before upload.",
    registerPendingHint: "Registration requires admin approval before login.",
    enterLoginServer: "Enter a login server",
    invalidCallsign: "Callsign must be 5-6 uppercase letters or digits",
    enterName: "Enter your name",
    invalidPhone: "Enter a valid phone number with at least 11 digits",
    enterPassword: "Enter a password",
    enterAddress: "Enter an address",
    invalidEmail: "Enter a valid email address",
    uploadLicense: "Upload your radio license photo",
    imageTooLarge: "Image is still too large after compression",
    imageReadFailed: "Image processing failed",
    registerFailed: "Registration failed. Please try again later.",
    registerSuccess: "Registration submitted. Please wait for review.",
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

// 悬浮窗双 PTT：各自独立的禁用条件与状态文案。
// 注意不含 busy：发射动作本身会短暂置 busy，含进去会让按钮在发射时闪禁止态（误导）。
const nrlPttDisabled = computed(() => nrlLinkState.value !== "online");
const fmoMqttConnected = computed(
  () => fmo.state.mqttState === "connected" || fmo.stats.mqttState === "connected",
);
const fmoPttDisabled = computed(() => !fmoMqttConnected.value);
const nrlStatusText = computed(() => {
  const zh = language.value === "zh";
  if (runtime.snapshot.connection === "recovering") return zh ? "重连中" : "Recovering";
  if (runtime.snapshot.connection === "connecting") return zh ? "连接中" : "Connecting";
  if (nrlLinkState.value === "online") return zh ? "在线" : "Online";
  if (nrlLinkState.value === "stale") return zh ? "断续" : "Weak";
  return zh ? "离线" : "Offline";
});
const fmoStatusText = computed(() => {
  const zh = language.value === "zh";
  if (fmo.state.mqttState === "connected") return zh ? "在线" : "Online";
  if (fmo.state.mqttState === "connecting") return zh ? "连接中" : "Connecting";
  return zh ? "离线" : "Offline";
});
const pttLinksLabel = computed(() => `NRL ${nrlStatusText.value} · FMO ${fmoStatusText.value}`);

// 语音互转（桥接）按钮：左 NRL 右 FMO，箭头方向即转发方向；关闭态为 SVG 双向箭头加斜杠图标。
// 模板按位渲染 ←（bit1：FMO→NRL）/ →（bit2：NRL→FMO）两个箭头，
// 对应方向有语音转发时（bridgeTxNrl/bridgeTxFmo）箭头闪烁。
// 文字版状态（aria-label / 读屏用）
const bridgeModeText = computed(() => {
  switch (runtime.snapshot.bridgeMode) {
    case 1:
      return "FMO→NRL";
    case 2:
      return "NRL→FMO";
    case 3:
      return "NRL↔FMO";
    default:
      return t.value.bridgeOff;
  }
});

// 悬浮窗各协议的当前说话人呼号（本机发射时显示本机呼号）
const nrlTalkerLabel = computed(() => {
  if (nrlPttActive.value) {
    return `${runtime.snapshot.callsign}-${runtime.snapshot.ssid}`;
  }
  return runtime.snapshot.activeSpeaker
    ? `${runtime.snapshot.activeSpeaker}-${runtime.snapshot.activeSpeakerSsid}`
    : "—";
});
const fmoTalkerLabel = computed(() => {
  if (fmoPttActive.value) return fmo.stats.callsign || "—";
  return fmo.stats.activeSpeaker || "—";
});
const currentTalker = computed(() => {
  if (isFmo.value) {
    // FMO 模式下 NRL 区只显示本机 NRL 呼号，互不干扰
    return `${runtime.config.callsign || "-"}-${runtime.config.ssid}`;
  }
  if (runtime.snapshot.connection !== "connected") {
    return "-";
  }
  if (nrlPttActive.value) {
    return `${runtime.snapshot.callsign}-${runtime.snapshot.ssid}`;
  }
  if (!runtime.snapshot.activeSpeaker) {
    return "---------";
  }
  return `${runtime.snapshot.activeSpeaker}-${runtime.snapshot.activeSpeakerSsid}`;
});
const currentTalkerRegion = computed(() => {
  if (isFmo.value) {
    return describeCallsignRegion(runtime.config.callsign);
  }
  if (runtime.snapshot.connection !== "connected") {
    return t.value.regionUnknown;
  }
  if (nrlPttActive.value) {
    return describeCallsignRegion(runtime.snapshot.callsign);
  }
  if (!runtime.snapshot.activeSpeaker) {
    return t.value.regionUnknown;
  }
  return describeCallsignRegion(runtime.snapshot.activeSpeaker);
});
const selectedNrlServerHost = computed(() => normalizeHost(runtime.config.server || platform.voiceServerHost));
const filteredNrlServers = computed(() => {
  const q = nrlServerSearch.value.trim().toLowerCase();
  if (!q) return platform.servers;
  return platform.servers.filter(
    (s) => s.name.toLowerCase().includes(q) || s.host.toLowerCase().includes(q),
  );
});
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
  const source = nrlPttActive.value
    ? runtime.snapshot.txSpectrum
    : runtime.snapshot.rxSpectrum;
  return Array.from({ length: 28 }, (_, index) => {
    const base = source[index] ?? 0;
    const shimmer = (Math.sin(animationTick.value * 0.022 + index * 0.52) + 1) * 0.025;
    return Math.min(1, Math.max(0.04, base + shimmer));
  });
});

// NRL box 内独立波形（28 频段）
const nrlSpectrumBars = computed(() => {
  const source = nrlPttActive.value
    ? runtime.snapshot.txSpectrum
    : runtime.snapshot.rxSpectrum;
  return Array.from({ length: 28 }, (_, index) => {
    const base = source[index] ?? 0;
    return Math.min(1, Math.max(0.04, base));
  });
});

// FMO box 内独立波形（28 频段，来自 fmo stats）
const fmoSpectrumBars = computed(() => {
  const source = fmoPttActive.value
    ? runtime.snapshot.txSpectrum
    : (fmo.stats.rxSpectrum ?? []);
  return Array.from({ length: 28 }, (_, index) => {
    const base = source[index] ?? 0;
    return Math.min(1, Math.max(0.04, base));
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

function formatBitRate(value: number): string {
  return `${Math.max(0, Math.round(value))} bit/s`;
}

const rxLevelDb = computed(() => formatDualDb(runtime.snapshot.rxLevel, rxPeakDisplay.value));

const txLevelDb = computed(() => formatDualDb(runtime.snapshot.txLevel, txPeakDisplay.value));

const fmoRxLevelDb = computed(() => formatDualDb(fmo.stats.rxLevel, fmoRxPeakDisplay.value));

const fmoTxLevelDb = computed(() => formatDualDb(fmo.stats.txLevel, fmoTxPeakDisplay.value));
const serialStatusText = computed(() => {
  if (!runtime.serialTunnel.supported) return t.value.serialUnsupported;
  if (runtime.serialTunnel.running) {
    return `${t.value.serialModePhysical} · ${runtime.serialTunnel.portName} · ${runtime.serialTunnel.status}`;
  }
  return "";
});
const serialPortOptions = computed(() => {
  return [...runtime.serialPorts];
});

function normalizedSerialDraft(): SerialTunnelConfig {
  return {
    ...serialTunnelDraft.value,
    mode: "physical",
    portName: serialTunnelDraft.value.portName.trim(),
    baudRate: Number(serialTunnelDraft.value.baudRate),
    dataBits: Number(serialTunnelDraft.value.dataBits),
  };
}

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

function stopRealtimeLoop() {
  if (realtimeRafId.value !== null) {
    window.cancelAnimationFrame(realtimeRafId.value);
    realtimeRafId.value = null;
  }
}

function stopClockTimer() {
  if (clockTimerId.value !== null) {
    window.clearInterval(clockTimerId.value);
    clockTimerId.value = null;
  }
}

function stopSerialRateTimer() {
  if (serialRateTimerId.value !== null) {
    window.clearInterval(serialRateTimerId.value);
    serialRateTimerId.value = null;
  }
}

function updateSerialRates() {
  const now = performance.now();
  const elapsedSec = Math.max(0.001, (now - serialRateLast.at) / 1000);
  const rxBytes = runtime.serialTunnel.rxBytes;
  const txBytes = runtime.serialTunnel.txBytes;
  const rxDelta = Math.max(0, rxBytes - serialRateLast.rxBytes);
  const txDelta = Math.max(0, txBytes - serialRateLast.txBytes);

  serialRxBps.value = (rxDelta * 8) / elapsedSec;
  serialTxBps.value = (txDelta * 8) / elapsedSec;
  serialRxActive.value = rxDelta > 0;
  serialTxActive.value = txDelta > 0;
  serialRateLast = { rxBytes, txBytes, at: now };
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

// NRL box 内 PTT：按下发射，松开停止
const nrlPttPressed = ref(false);
const fmoPttPressed = ref(false);
const nrlPttActive = computed(
  () => nrlPttPressed.value
    || (runtime.snapshot.isTransmitting && runtime.snapshot.txProtocol === "nrl")
    || runtime.snapshot.bridgeTxNrl,
);
const fmoPttActive = computed(
  () => fmoPttPressed.value
    || (runtime.snapshot.isTransmitting && runtime.snapshot.txProtocol === "fmo")
    || runtime.snapshot.bridgeTxFmo,
);

async function pressNrlPtt(event?: PointerEvent) {
  if (runtime.busy || nrlPttPressed.value || runtime.snapshot.isTransmitting || nrlLinkState.value !== "online") {
    return;
  }
  if (event?.currentTarget instanceof Element) {
    try { event.currentTarget.setPointerCapture(event.pointerId); } catch { /* ok */ }
  }
  nrlPttPressed.value = true;
  await runtime.setTxProto("nrl", true);
}

async function releaseNrlPtt() {
  if (!nrlPttPressed.value) return;
  nrlPttPressed.value = false;
  await runtime.setTxProto("nrl", false);
}

async function pressFmoPtt(event?: PointerEvent) {
  if (fmoPttPressed.value || runtime.snapshot.isTransmitting || !fmoMqttConnected.value) {
    return;
  }
  if (event?.currentTarget instanceof Element) {
    try { event.currentTarget.setPointerCapture(event.pointerId); } catch { /* ok */ }
  }
  fmoPttPressed.value = true;
  if (!await runtime.setTxProto("fmo", true)) {
    fmoPttPressed.value = false;
  }
}

async function releaseFmoPtt() {
  if (!fmoPttPressed.value) return;
  fmoPttPressed.value = false;
  await runtime.setTxProto("fmo", false);
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
    serialTunnel: normalizedSerialDraft(),
  });
}

async function startSerialTunnelUi() {
  try {
    await runtime.startSerial(normalizedSerialDraft());
  } catch (error) {
    alert(error instanceof Error ? error.message : String(error));
  }
}

async function stopSerialTunnelUi() {
  await runtime.stopSerial();
}

async function refreshSerialPortsUi() {
  await runtime.refreshSerialPorts();
  if (!runtime.serialPorts.includes(serialTunnelDraft.value.portName)) {
    serialTunnelDraft.value.portName = "";
  }
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
  registerSuccess.value = "";
  try {
    await platform.login();
    showRegister.value = false;
    showTokenLogin.value = false;
    showLogin.value = false;
  } catch (error) {
    loginError.value = error instanceof Error ? error.message : String(error);
  }
}

async function loginPlatformWithToken() {
  loginError.value = "";
  registerSuccess.value = "";
  const token = platform.hamidToken.trim();
  if (!token) {
    loginError.value = t.value.enterHamidToken;
    return;
  }
  if (!token.startsWith("hamid_pat_") || token.trim().length <= "hamid_pat_".length) {
    loginError.value = t.value.invalidHamidToken;
    return;
  }
  try {
    await platform.loginWithToken(token);
    showRegister.value = false;
    showTokenLogin.value = false;
    showLogin.value = false;
  } catch (error) {
    loginError.value = error instanceof Error ? error.message : String(error);
  }
}

async function submitRegister() {
  resetRegisterState();
  const host = resolveAuthHost();
  if (!host) {
    registerError.value = t.value.enterLoginServer;
    return;
  }

  const payload: PlatformRegisterPayload = {
    callsign: registerForm.value.callsign.trim().toUpperCase(),
    name: registerForm.value.name.trim(),
    phone: registerForm.value.phone.trim(),
    password: registerForm.value.password,
    address: registerForm.value.address.trim(),
    mail: registerForm.value.mail.trim(),
  };

  if (!/^[A-Z0-9]{5,6}$/.test(payload.callsign)) {
    registerError.value = t.value.invalidCallsign;
    return;
  }
  if (!payload.name) {
    registerError.value = t.value.enterName;
    return;
  }
  if (!/^\d{11,}$/.test(payload.phone)) {
    registerError.value = t.value.invalidPhone;
    return;
  }
  if (!payload.password) {
    registerError.value = t.value.enterPassword;
    return;
  }
  if (!payload.address) {
    registerError.value = t.value.enterAddress;
    return;
  }
  if (!/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(payload.mail)) {
    registerError.value = t.value.invalidEmail;
    return;
  }
  if (!registerLicense.value) {
    registerError.value = t.value.uploadLicense;
    return;
  }

  registerBusy.value = true;
  try {
    const result = await platformRegister(
      host,
      payload,
      registerLicense.value.name,
      registerLicense.value.bytes,
    );
    if (result.code !== 20000) {
      registerError.value = result.message || t.value.registerFailed;
      return;
    }
    registerSuccess.value = t.value.registerSuccess;
    resetRegisterForm();
    showRegister.value = false;
  } catch (error) {
    registerError.value = error instanceof Error ? error.message : String(error);
  } finally {
    registerBusy.value = false;
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
  serialTunnelDraft.value = { ...runtime.config.serialTunnel };
}

// ---------------------------------------------------------------- FMO

const fmoServerName = (s: FmoServer | null | undefined) =>
  (s?.name || s?.callsign || s?.host || "未选定").toString();

async function setProtocol(next: "nrl" | "fmo") {
  const wasFmo = isFmo.value;
  if (next === wasFmo) {
    return;
  }
  if (runtime.snapshot.connection === "connected") {
    await runtime.disconnect();
  }
  await runtime.saveConfig({ ...runtime.config, protocol: next });
  fmoMuted.value = false;
}

async function toggleFmoMute() {
  fmoMuted.value = !fmoMuted.value;
  await fmo.setRxPlay(!fmoMuted.value);
}

async function toggleFmoNoLocal(event: Event) {
  const target = event.target as HTMLInputElement;
  await fmo.setMqttNoLocal(target.checked);
}

async function onFmoCertSlotChange(name: string) {
  fmoCertMsg.value = "";
  try {
    // 用 dialog 打开文件选择器，返回真实本地路径（input[type=file] 在 Tauri 拿不到路径）
    const selected = await open({
      multiple: false,
      title: language.value === "zh" ? `选择 ${name} 证书文件` : `Select ${name} cert file`,
      filters: [
        {
          name: "JSON",
          extensions: ["json"],
        },
      ],
    });
    if (!selected || typeof selected !== "string") {
      return;
    }
    // 明确指定证书类型 name（cert_user/cert_int/cert_root/cert_devicekey）
    const result = (await fmo.importCertFile(selected, name)) as
      | { name?: string; identity_check?: { checked: boolean; ok?: boolean; msg?: string } }
      | undefined;
    // 导入后立即刷新：身份/呼号/UID/passcode/证书列表
    await fmo.refresh();
    const certLabel = fmoCertSlots.find((s) => s.name === name)?.label ?? name;
    const ready = fmoCertReadyCount.value;
    // 身份一致性检查不通过（证书不是一套/已过期）：优先展示警告
    const check = result?.identity_check;
    if (check?.checked && !check.ok) {
      fmoCertMsg.value = language.value === "zh"
        ? `⚠ ${check.msg}`
        : `⚠ Identity check failed: ${check.msg}`;
      return;
    }
    const matchHint = check?.checked && check.ok
      ? (language.value === "zh" ? " · ✓ 私钥与证书匹配" : " · ✓ key matches cert")
      : "";
    if (name === "cert_user") {
      const cs = fmo.state.identity.callsign;
      const uid = fmo.state.identity.uid;
      fmoCertMsg.value = cs
        ? (language.value === "zh"
            ? `✓ ${certLabel} 导入成功：呼号 ${cs} · UID ${uid} · passcode ${fmo.state.passcode}${matchHint}`
            : `✓ ${certLabel} imported: ${cs} · UID ${uid} · passcode ${fmo.state.passcode}${matchHint}`)
        : (language.value === "zh"
            ? `✓ ${certLabel} 已导入，但未解析到呼号，请确认文件内容`
            : `✓ ${certLabel} imported, but no callsign parsed`);
    } else {
      fmoCertMsg.value = language.value === "zh"
        ? `✓ ${certLabel} 导入成功（${ready}/4）${matchHint}`
        : `✓ ${certLabel} imported (${ready}/4)${matchHint}`;
    }
  } catch (e) {
    fmoCertMsg.value = String(e);
  }
}

async function loadFmoActivateConfig() {
  try {
    const cfg = await fmo.activateGetConfig();
    fmoActivateServer.value = cfg.server;
    fmoActivateMac.value = cfg.mac;
  } catch (e) {
    flog("[fmo] activate config error:", String(e));
  }
}

async function saveFmoActivateServer() {
  try {
    await fmo.activateSetConfig(fmoActivateServer.value.trim());
    fmoActivateMsg.value = language.value === "zh" ? "✓ 证书服务器地址已保存" : "✓ Server saved";
  } catch (e) {
    fmoActivateMsg.value = String(e);
  }
}

async function runFmoActivate() {
  fmoActivateMsg.value = "";
  fmoActivating.value = true;
  try {
    const msg = await fmo.activateRun();
    fmoActivateMsg.value = `✓ ${msg}`;
  } catch (e) {
    fmoActivateMsg.value = String(e);
  } finally {
    fmoActivating.value = false;
  }
}

async function connectFmoAprs() {
  const cs = (fmo.state.identity.callsign || fmo.state.certCallsign || fmoAprsCallsign.value || "").trim();
  if (!cs) {
    alert(language.value === "zh" ? "请输入 FMO 呼号（或先导入证书）" : "Enter FMO callsign");
    return;
  }
  try {
    await fmo.connectAprs(cs);
  } catch (e) {
    alert(String(e));
  }
}

async function selectFmoServerAndConnect(s: FmoServer) {
  // 已连接时后端 select_server 内部会自动断开重连；未连接时这里补一次显式连接，
  // 保证「点列表 = 登录该服务器」。
  try {
    await fmo.selectServer(s);
    const st = fmo.state.mqttState;
    if (st !== "connected" && st !== "connecting") {
      await connectFmoMqtt();
    }
  } catch (e) {
    alert(String(e));
  }
}

async function connectFmoMqtt() {
  // 无选定服务器时自动兜底：第一台收藏 → 在线数最高的服务器
  let sel = fmo.selectedServer();
  if (!sel) {
    const cand =
      (fmo.state.favorites[0] as unknown as FmoServer | undefined) ??
      sortedFmoServers.value[0];
    if (cand) {
      await fmo.selectServer(cand);
      sel = cand;
    }
  }
  if (!sel) {
    alert(language.value === "zh"
      ? "暂无可用服务器，请先连接 APRS 发现服务器"
      : "No server available yet. Connect APRS to discover servers.");
    return;
  }
  try {
    // FMO 专用 MQTT 连接（fmo_mqtt_connect 内部会确保音频引擎启动）
    await fmo.connectMqtt();
  } catch (e) {
    alert(String(e));
  }
}

async function toggleFmoServerFavorite(s: FmoServer) {
  const key = `${s.host}:${s.port ?? 1883}`;
  const existing = fmo.state.favorites.find((f) => f.key === key);
  if (existing) {
    await fmo.removeFavorite(key);
  } else {
    await fmo.addFavorite(s);
  }
}

// 群组在线设备弹窗
const groupDevicesPopup = ref<{ group: PlatformGroup; devices: PlatformDevice[] } | null>(null);
const groupDevicesLoading = ref(false);

async function showGroupDevices(group: PlatformGroup) {
  if (!platform.apiBase || !platform.token) {
    return;
  }
  groupDevicesPopup.value = { group, devices: [] };
  groupDevicesLoading.value = true;
  try {
    const devices = await platformFetchGroupDevices(platform.apiBase, platform.token, group.id);
    groupDevicesPopup.value = { group, devices };
  } catch (e) {
    groupDevicesPopup.value = { group, devices: [] };
  } finally {
    groupDevicesLoading.value = false;
  }
}

function isFmoServerFavorited(s: FmoServer): boolean {
  const key = `${s.host}:${s.port ?? 1883}`;
  return fmo.state.favorites.some((f) => f.key === key);
}

const fmoMqttText = computed(() => {
  const map: Record<string, string> = {
    connected: language.value === "zh" ? "已连接" : "Connected",
    connecting: language.value === "zh" ? "连接中" : "Connecting",
    error: language.value === "zh" ? "错误" : "Error",
    disconnected: language.value === "zh" ? "未连接" : "Disconnected",
  };
  return map[fmo.state.mqttState] ?? fmo.state.mqttState;
});

const fmoAprsText = computed(() => {
  const map: Record<string, string> = {
    verified: language.value === "zh" ? "已校验" : "Verified",
    "logged-in": language.value === "zh" ? "已登录" : "Logged In",
    "listen-only": language.value === "zh" ? "仅收听" : "Listen-Only",
    connecting: language.value === "zh" ? "连接中" : "Connecting",
    disconnected: language.value === "zh" ? "未连接" : "Disconnected",
  };
  return map[fmo.state.aprsState] ?? fmo.state.aprsState;
});

// APRS 已建立连接（已校验/已登录/仅收听均视为在线），驱动标签加亮
const fmoAprsOnline = computed(() =>
  ["verified", "logged-in", "listen-only"].includes(fmo.state.aprsState),
);

// FMO 服务器列表：按在线设备数从高到低排序
const sortedFmoServers = computed(() =>
  [...fmo.state.servers].sort((a, b) => (b.online ?? 0) - (a.online ?? 0)),
);

// 服务器/用户 Tab 的筛选关键字（参照 sim 前端的 srv-filter / cli-filter）
const fmoServerFilter = ref("");
const fmoUserFilter = ref("");
const filteredFmoServers = computed(() => {
  const q = fmoServerFilter.value.trim().toLowerCase();
  if (!q) return sortedFmoServers.value;
  return sortedFmoServers.value.filter((s) =>
    [s.name, s.callsign, s.host]
      .filter(Boolean)
      .some((v) => String(v).toLowerCase().includes(q)),
  );
});
const filteredFmoClients = computed(() => {
  const q = fmoUserFilter.value.trim().toLowerCase();
  if (!q) return fmo.state.clients;
  return fmo.state.clients.filter((c) =>
    [c.callsign, c.status_text, c.comment, c.uid ? String(c.uid) : ""]
      .filter(Boolean)
      .some((v) => String(v).toLowerCase().includes(q)),
  );
});

// FMO 用户信标时间显示（last_seen 为 unix 秒）
function fmtClientTime(ts?: number): string {
  if (!ts) return "—";
  const d = new Date(ts * 1000);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

// 用户行主信息已显示最新状态文本，最近消息里与之相同的条目不再重复展示
function fmoClientRecentExtras(c: FmoClient): { ts: number; text: string }[] {
  const shown = c.status_text || c.comment || "";
  return (c.recent ?? []).filter((m) => m.text !== shown);
}

// 用户行内联概要：状态文本之外的附加信息（频率/电台/天线/高度/位置）
function fmoClientDetailLine(c: FmoClient): string {
  const parts: string[] = [];
  if (c.freq) parts.push(`${c.freq.toFixed(4)} MHz`);
  if (c.rig) parts.push(c.rig);
  if (c.ant) parts.push(c.ant);
  if (c.height != null) parts.push(`${c.height}m`);
  if (c.lat && c.lon) parts.push(`${c.lat} ${c.lon}`);
  return parts.join(" · ");
}

// 完整日期时间（弹窗里的首次/最后出现）
function fmtClientDateTime(ts?: number): string {
  if (!ts) return "—";
  const d = new Date(ts * 1000);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

// 用户详情弹窗
const fmoUserPopup = ref<FmoClient | null>(null);
const fmoUserDetailRows = computed(() => {
  const c = fmoUserPopup.value;
  if (!c) return [] as { label: string; value: string }[];
  const zh = language.value === "zh";
  const rows: { label: string; value: string }[] = [];
  const push = (label: string, value: string | number | undefined | null) => {
    if (value !== undefined && value !== null && String(value) !== "") {
      rows.push({ label, value: String(value) });
    }
  };
  push(zh ? "呼号" : "Callsign", c.callsign);
  push("UID", c.uid);
  push(zh ? "类型" : "Type", [c.kind, c.subtype, c.version].filter(Boolean).join(" / "));
  push(zh ? "状态文本" : "Status", c.status_text);
  push(zh ? "位置注释" : "Comment", c.comment);
  push(zh ? "频率" : "Freq", c.freq ? `${c.freq.toFixed(4)} MHz` : undefined);
  push(zh ? "电台" : "Rig", c.rig);
  push(zh ? "天线" : "Antenna", c.ant);
  push(zh ? "高度" : "Height", c.height != null ? `${c.height} m` : undefined);
  push(zh ? "位置" : "Position", c.lat && c.lon ? `${c.lat} ${c.lon}` : undefined);
  push(zh ? "首次出现" : "First seen", fmtClientDateTime(c.first_seen));
  push(zh ? "最后出现" : "Last seen", fmtClientDateTime(c.last_seen));
  return rows;
});

// 服务器行内联概要：呼号 + 状态文本 + 频率/高度/电台/天线/位置
function fmoServerDetailLine(s: FmoServer): string {
  const parts: string[] = [];
  if (s.callsign && s.callsign !== s.name) parts.push(s.callsign);
  if (s.status_text) parts.push(s.status_text);
  if (s.freq) parts.push(`${s.freq.toFixed(4)} MHz`);
  if (s.height != null) parts.push(`${s.height}m`);
  if (s.rig) parts.push(s.rig);
  if (s.ant) parts.push(s.ant);
  if (s.lat && s.lon) parts.push(`${s.lat} ${s.lon}`);
  return parts.join(" · ");
}

// 服务器详情弹窗（收藏条目先回查完整服务器信息，查不到再用收藏自身字段）
const fmoServerPopup = ref<FmoServer | null>(null);
function openFmoServerPopup(s: FmoServer) {
  const full = fmo.state.servers.find((x) => x.key === s.key);
  fmoServerPopup.value = full ?? s;
}
const fmoServerDetailRows = computed(() => {
  const s = fmoServerPopup.value;
  if (!s) return [] as { label: string; value: string }[];
  const zh = language.value === "zh";
  const rows: { label: string; value: string }[] = [];
  const push = (label: string, value: string | number | undefined | null) => {
    if (value !== undefined && value !== null && String(value) !== "") {
      rows.push({ label, value: String(value) });
    }
  };
  push(zh ? "名称" : "Name", s.name);
  push(zh ? "呼号" : "Callsign", s.callsign);
  push("UID", s.uid);
  push(zh ? "地址" : "Address", s.host ? `${s.host}:${s.port ?? "?"}` : undefined);
  push(zh ? "状态文本" : "Status", s.status_text);
  push(zh ? "频率" : "Freq", s.freq ? `${s.freq.toFixed(4)} MHz` : undefined);
  push(zh ? "高度" : "Height", s.height != null ? `${s.height} m` : undefined);
  push(zh ? "电台" : "Rig", s.rig);
  push(zh ? "天线" : "Antenna", s.ant);
  push(zh ? "覆盖" : "Coverage", s.cover_km ? `${s.cover_km} km` : undefined);
  push(zh ? "在线" : "Online", s.online != null ? `${s.online} / ${s.total ?? "?"} ${zh ? "峰值" : "peak"}` : undefined);
  push(zh ? "位置" : "Position", s.lat && s.lon ? `${s.lat} ${s.lon}` : undefined);
  push(zh ? "国家/地区" : "Country", s.country);
  push(zh ? "版本" : "Version", [s.subtype, s.version].filter(Boolean).join(" / ") || undefined);
  push(zh ? "来源" : "Source", s.source);
  push(zh ? "最后出现" : "Last seen", s.last_seen ? fmtClientDateTime(s.last_seen) : undefined);
  return rows;
});

// ---------------------------------------------------------------- FMO QSO / 服务器广播

// QSO 呼叫弹窗
const qsoDialogOpen = ref(false);
const qsoTargetCallsign = ref("");
const qsoTargetUid = ref<number | null>(null);

const qsoPhaseText = computed(() => {
  const zh = language.value === "zh";
  const map: Record<string, string> = zh
    ? {
        idle: "空闲", querying: "查询对方服务器…", calling: "呼叫中…",
        ringing: "对方振铃中…", incoming: "来电", established: "QSO 已建立",
      }
    : {
        idle: "Idle", querying: "Querying server…", calling: "Calling…",
        ringing: "Ringing…", incoming: "Incoming", established: "QSO established",
      };
  return map[fmo.qso.phase] ?? fmo.qso.phase;
});

function pickQsoTarget(c: FmoClient) {
  qsoTargetCallsign.value = c.callsign;
  qsoTargetUid.value = c.uid ?? null;
}

async function startQsoCall() {
  try {
    await fmo.qsoCall(qsoTargetCallsign.value.trim(), qsoTargetUid.value ?? undefined);
  } catch (e) {
    alert(String(e));
  }
}

async function answerQso(accept: boolean) {
  try {
    await fmo.qsoAnswer(accept);
  } catch (e) {
    alert(String(e));
  }
}

async function cancelQso() {
  try {
    await fmo.qsoCancel();
  } catch (e) {
    alert(String(e));
  }
}

// QSO 记录（设置页展示最近 10 条，新的在前）
const qsoLogRecent = computed(() => [...fmo.qsoLog].reverse().slice(0, 10));

// 成功接通的 QSO 列表（右侧 QSO 栏，新的在前；含收到的完整通联记录/祝福）
const qsoSuccessList = computed(() =>
  [...fmo.qsoLog]
    .filter((r) => r.result === "接通" || r.result.startsWith("已接听") || r.result === "通联记录")
    .reverse(),
);

// 从 QSO 记录直接发起再次呼叫
function qsoCallAgain(r: { peer: string; peer_uid: number }) {
  qsoTargetCallsign.value = r.peer;
  qsoTargetUid.value = r.peer_uid || null;
  qsoDialogOpen.value = true;
}

function fmtQsoTime(ts: number): string {
  const d = new Date(ts * 1000);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

// 服务器广播配置草稿（设置页编辑，保存才下发后端）
const broadcastDraft = ref<FmoBroadcastConfig>({ ...fmo.broadcast });
watch(
  () => fmo.broadcast,
  (v) => {
    broadcastDraft.value = { ...v };
  },
);

async function saveBroadcastConfig() {
  try {
    await fmo.saveBroadcast({ ...broadcastDraft.value });
    alert(language.value === "zh" ? "广播配置已保存" : "Broadcast config saved");
  } catch (e) {
    // 后端拒绝时（如 super 门控不满足）把中文原因直接展示给用户
    alert(String(e));
  }
}

// 把当前选定服务器的 host/port 带入广播草稿（广播的正是自己服务器的地址）
function fillBroadcastFromServer() {
  const srv = fmo.selectedServer();
  if (!srv) return;
  if (srv.host) broadcastDraft.value.host = String(srv.host);
  if (srv.port) broadcastDraft.value.port = Number(srv.port);
}

async function manualBroadcast() {
  try {
    await fmo.broadcastNow();
  } catch (e) {
    alert(String(e));
  }
}

// 个人信标（BEACON）配置草稿（设置页编辑，保存才下发后端）
const beaconDraft = ref<FmoBeaconConfig>({ ...fmo.beacon });
watch(
  () => fmo.beacon,
  (v) => {
    beaconDraft.value = { ...v };
  },
);

async function saveBeaconConfig() {
  try {
    await fmo.saveBeacon({ ...beaconDraft.value });
    alert(language.value === "zh" ? "信标配置已保存" : "Beacon config saved");
  } catch (e) {
    // 后端拒绝时（字段校验失败）把中文原因直接展示给用户
    alert(String(e));
  }
}

async function manualBeacon() {
  try {
    await fmo.beaconNow();
  } catch (e) {
    alert(String(e));
  }
}

// 当前 FMO 说话人在 APRS 用户表中匹配到的信标信息（先精确匹配呼号，再退化为不含 SSID 的主呼号）
const fmoSpeakerClient = computed<FmoClient | null>(() => {
  const spk = (fmo.stats.activeSpeaker || "").toUpperCase();
  if (!spk) return null;
  const list = fmo.state.clients;
  const base = spk.split("-")[0];
  return (
    list.find((c) => c.callsign.toUpperCase() === spk) ??
    list.find((c) => c.callsign.toUpperCase().split("-")[0] === base) ??
    null
  );
});
// 匹配到时在呼号面板上展示的附加信息行：状态文本一行，频率/电台/天线/高度一行
const fmoSpeakerInfoLines = computed<string[]>(() => {
  const c = fmoSpeakerClient.value;
  if (!c) return [];
  const lines: string[] = [];
  const status = c.status_text || c.comment || "";
  if (status) lines.push(status);
  const details: string[] = [];
  if (c.freq) details.push(`${c.freq.toFixed(4)} MHz`);
  if (c.rig) details.push(c.rig);
  if (c.ant) details.push(c.ant);
  if (c.height != null) details.push(`${c.height}m`);
  if (details.length) lines.push(details.join(" · "));
  return lines;
});

// 说话人位置行（后端已解算，对齐原厂固件）：网格 · 距离 · 罗盘方位。
// 位置来源 beacon（APRS 信标经纬度）精确；grid（成员 JSON 网格）±10km，距离前加 ≈。
const fmoSpeakerGeoLine = computed<string>(() => {
  const s = fmo.stats;
  if (s.speakerDistanceKm == null && !s.speakerGrid) return "";
  const parts: string[] = [];
  if (s.speakerGrid) parts.push(s.speakerGrid);
  if (s.speakerDistanceKm != null) {
    const approx = s.speakerPosSource === "grid" ? "≈" : "";
    parts.push(`${approx}${s.speakerDistanceKm} km`);
  }
  if (s.speakerCompass) {
    const deg = s.speakerBearingDeg != null ? ` ${s.speakerBearingDeg.toFixed(0)}°` : "";
    parts.push(`${s.speakerCompass}${deg}`);
  }
  return parts.join(" · ");
});

// 语音活动检测：接收帧序号/计数变化视为有语音进来，900ms 内在大呼号右下角显示编码角标
const nrlLastVoiceAt = ref(0);
const fmoLastVoiceAt = ref(0);
watch(
  () => runtime.snapshot.rxSeq,
  (seq) => {
    if (seq) nrlLastVoiceAt.value = Date.now();
  },
);
watch(
  () => fmo.stats.rxFrames,
  (n) => {
    if (n) fmoLastVoiceAt.value = Date.now();
  },
);
// NRL 链路状态：10s 内收到过 UDP 报文（语音/心跳等）为在线点亮，超过则闪烁告警
const nrlLinkState = computed<"off" | "online" | "stale">(() => {
  void animationTick.value; // 跟随 rAF 渲染循环周期性重估
  if (runtime.snapshot.connection === "disconnected") return "off";
  if (["connecting", "recovering"].includes(runtime.snapshot.connection)) return "stale";
  const last = runtime.snapshot.nrlLastRxMs ?? 0;
  if (!last) return "off";
  return Date.now() - last < 10_000 ? "online" : "stale";
});

const nrlVoiceActive = computed(() => {
  void animationTick.value; // 跟随 rAF 渲染循环周期性重估
  return Date.now() - nrlLastVoiceAt.value < 900;
});
const fmoVoiceActive = computed(() => {
  void animationTick.value;
  return Date.now() - fmoLastVoiceAt.value < 900;
});

onMounted(async () => {
  try {
    const version = await getVersion();
    const title = `NRL Pulse v${version} © BH4RPN 2026 , BA4RN BG6FCS BH4TDV BD4RFG BD4VKI BI4UMD BA4QAO BA4QGT ...  `;
    document.title = title;
    await getCurrentWindow().setTitle(title);
  } catch { /* 权限未授予时不影响后续初始化 */ }
  if (isMonitorWindow) {
    document.documentElement.classList.add("monitor-window");
    document.body.classList.add("monitor-window");
    return;
  }
  if (isPttWindow) {
    document.documentElement.classList.add("ptt-window");
    document.body.classList.add("ptt-window");
  }
  await runtime.bootstrap();
  await fmo.bootstrap();
  await loadFmoActivateConfig();
  defaultAudioPath.value = await getDefaultAudioDir();
  if (!isPttWindow) {
    // NRL 设备心跳不依赖登录/平台服务器列表；先用上次保存的 NRL 服务器自动连接。
    if (runtime.config.protocol !== "fmo" && runtime.snapshot.connection === "disconnected") {
      void runtime.connect();
    }

    // 平台登录态/服务器列表只影响管理功能，加载失败不能阻塞语音自动连接。
    try {
      await platform.bootstrap();
    } catch (error) {
      flog("[platform] bootstrap failed:", String(error));
    }
  }
  syncConfigDrafts();
  if (!isPttWindow && !platform.loggedIn && !isFmo.value) {
    showLogin.value = false;
  }
  await onChatMessage((event) => {
    appendChatMessage(event);
  });
  window.addEventListener("keydown", handleGlobalKeydown);
  window.addEventListener("keyup", handleGlobalKeyup);
  // 实时仪表/波形统一走 rAF 渲染循环：按屏幕刷新率绘制，数据事件只更新 store，
  // 帧率不再受后端事件节奏限制（FMO 数据 240ms 突发到达也能平滑显示）
  const realtimeLoop = () => {
    animationTick.value += 1;
    redrawRealtimeCanvases();
    realtimeRafId.value = window.requestAnimationFrame(realtimeLoop);
  };
  realtimeRafId.value = window.requestAnimationFrame(realtimeLoop);
  clockTimerId.value = window.setInterval(() => {
    currentTime.value = new Date();
  }, 1000);
  serialRateLast = {
    rxBytes: runtime.serialTunnel.rxBytes,
    txBytes: runtime.serialTunnel.txBytes,
    at: performance.now(),
  };
  serialRateTimerId.value = window.setInterval(updateSerialRates, 500);
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
  stopRealtimeLoop();
  stopClockTimer();
  stopSerialRateTimer();
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

// 日志整行按类型着色：色键 = 标题 + 详情前缀（到首个 : 或 " 为止），
// 使 "FMO 事件" 下的 TELE/广播/状态/APRS 等子类型各有固定颜色。
// warn/accent 级别色优先（见 CSS），info 行返回三档亮度的同系色。
interface LogLineColors {
  time: string;
  title: string;
  detail: string;
}

function logLineColors(entry: TimelineEvent): LogLineColors | undefined {
  if (entry.tone !== "info") return undefined;
  const prefix = entry.detail.match(/^[^:"]{0,24}/)?.[0] ?? "";
  const key = `${entry.title}|${prefix}`;
  let hash = 0;
  for (let i = 0; i < key.length; i++) {
    hash = (hash * 31 + key.charCodeAt(i)) >>> 0;
  }
  // 色相限制在 100°–330°（绿/青/蓝/紫），避开红橙黄，防止和 warn/accent 告警色混淆
  const hue = 100 + (hash % 230);
  return {
    time: `hsl(${hue} 45% 52%)`,
    title: `hsl(${hue} 78% 72%)`,
    detail: `hsl(${hue} 55% 62%)`,
  };
}

// 内嵌滚动日志：时间正序（新日志在底部），每行预计算类型颜色
const logEntries = computed(() =>
  [...runtime.timeline]
    .reverse()
    .map((entry) => ({ entry, colors: logLineColors(entry) })),
);

function handleLogScroll() {
  const el = logListEl.value;
  if (!el) return;
  logFollowBottom.value = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
}

function scrollLogToBottom() {
  const el = logListEl.value;
  if (el) el.scrollTop = el.scrollHeight;
}

watch(
  [() => runtime.timeline.length, chatTab],
  async () => {
    if (chatTab.value !== "logs" || !logFollowBottom.value) return;
    await nextTick();
    scrollLogToBottom();
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
  () => runtime.serialPorts,
  (ports) => {
    if (serialTunnelDraft.value.portName && !ports.includes(serialTunnelDraft.value.portName)) {
      serialTunnelDraft.value.portName = "";
    }
  },
  { deep: true },
);

watch(
  () => runtime.snapshot.connection,
  (state, previous) => {
    // 设备心跳建档后，可能已被管理员迁移到某个房间。
    // 连接建立时同步一次当前设备所在房间，避免登录后总是显示公共大厅。
    if (state === "connected" && previous !== "connected" && protocol.value === "nrl") {
      void platform.syncCurrentDeviceGroup();
    }
  },
);

watch(
  () => platform.loggedIn,
  (loggedIn) => {
    // 登录态只用于管理功能；不要因未登录强制打断主界面的服务器选择/语音连接。
    if (!loggedIn) {
      showLogin.value = false;
    }
  },
);
</script>

<template>
  <MonitorWindow v-if="isMonitorWindow" />

  <main v-if="isPttWindow" class="shell shell-ptt">
    <section
      class="ptt-console"
      :class="{
        'is-tx': runtime.snapshot.isTransmitting,
        'is-tx-nrl': nrlPttActive,
        'is-tx-fmo': fmoPttActive,
      }"
    >
      <header class="ptt-console-head" @pointerdown="onHeaderPointerDown">
        <span class="ptt-status-led" :data-state="runtime.snapshot.connection"></span>
        <span class="ptt-status-text">{{ pttLinksLabel }}</span>
        <span class="ptt-grip" aria-hidden="true"></span>
        <button
          class="ptt-console-close"
          :title="t.closePttWindow"
          @pointerdown.stop
          @click.stop="closeFloatingWindow"
        >×</button>
      </header>

      <div class="ptt-console-body">
        <div class="ptt-dial-slot">
          <button
            class="ptt-dial ptt-dial-nrl"
            :class="{ active: nrlPttActive, pressed: nrlPttActive, disabled: nrlPttDisabled }"
            @pointerdown.prevent="pressNrlPtt($event)"
            @pointerup.prevent="releaseNrlPtt()"
            @pointercancel.prevent="releaseNrlPtt()"
          >
            <span class="ptt-dial-halo"></span>
            <span class="ptt-dial-ring"></span>
            <span class="ptt-dial-core">
              <span class="ptt-dial-label">NRL</span>
              <span class="ptt-dial-sub">PTT</span>
            </span>
          </button>
          <strong class="ptt-dial-talker">{{ nrlTalkerLabel }}</strong>
          <span class="ptt-dial-status" :class="{ online: nrlLinkState === 'online' }">
            {{ nrlStatusText }}
          </span>
        </div>

        <div class="ptt-dial-slot">
          <button
            class="ptt-dial ptt-dial-fmo"
            :class="{ active: fmoPttActive, pressed: fmoPttActive, disabled: fmoPttDisabled }"
            @pointerdown.prevent="pressFmoPtt($event)"
            @pointerup.prevent="releaseFmoPtt()"
            @pointercancel.prevent="releaseFmoPtt()"
          >
            <span class="ptt-dial-halo"></span>
            <span class="ptt-dial-ring"></span>
            <span class="ptt-dial-core">
              <span class="ptt-dial-label">FMO</span>
              <span class="ptt-dial-sub">PTT</span>
            </span>
          </button>
          <strong class="ptt-dial-talker">{{ fmoTalkerLabel }}</strong>
          <span class="ptt-dial-status" :class="{ online: fmo.state.mqttState === 'connected' }">
            {{ fmoStatusText }}
          </span>
        </div>
      </div>
    </section>
  </main>

  <main v-else-if="!isMonitorWindow" class="shell">
    <!-- 顶部菜单导航栏：语言/登录/配置/更新 -->
    <header class="menu-bar">
      <div class="menu-bar-brand">
        <strong>NRL Pulse</strong>
        <span class="menu-bar-sub">{{ language === "zh" ? "网络电台客户端" : "Network Radio Client" }}</span>
      </div>
      <nav class="topbar-actions menu-bar-actions">
        <button class="ghost-btn" :title="language === 'zh' ? '所有房间监听' : 'All Room Monitor'" @click="openMonitorWindow()">
          {{ language === "zh" ? "监听" : "Monitor" }}
        </button>
        <button class="ghost-btn lang-btn" @click="toggleLanguage">
          {{ language === "zh" ? "EN" : "中" }}
        </button>
        <button
          class="ghost-btn"
          :class="{ 'status-connected': platform.loggedIn }"
          :disabled="platform.busy"
          @click="showLogin = !showLogin"
        >
          {{ platform.loggedIn ? t.platformLoggedIn : t.platformLogin }}
        </button>
        <button class="ghost-btn" :disabled="runtime.busy" @click="showSettings = !showSettings">
          {{ showSettings ? t.closeSettings : t.openSettings }}
        </button>
        <button class="ghost-btn" @click="manualCheckUpdate">
          {{ t.checkUpdate }}
        </button>
      </nav>
    </header>

    <header class="topbar">
      <div class="topbar-summary">
        <div class="summary-item summary-callsign">
          <span>NRL {{ t.localCallsign }}</span>
          <strong>{{ uiActive ? `${runtime.config.callsign}-${runtime.config.ssid}` : "-" }}</strong>
        </div>
        <div class="summary-item">
          <span>{{ t.latency }}</span>
          <strong>{{ uiActive ? runtime.snapshot.latencyMs : 0 }} ms</strong>
        </div>
        <div class="summary-item">
          <span>{{ t.jitter }}</span>
          <strong>{{ uiActive ? runtime.snapshot.jitterMs : 0 }} ms</strong>
        </div>
        <div class="summary-item">
          <span>{{ t.loss }}</span>
          <strong>{{ uiActive ? runtime.snapshot.packetLoss.toFixed(1) : "0.0" }}%</strong>
        </div>
        <div class="summary-item">
          <span>{{ t.queue }}</span>
          <strong>{{ uiActive ? runtime.snapshot.queuedFrames : 0 }}</strong>
        </div>
        <div class="summary-item summary-signal nrl-vu">
          <div class="signal-stack">
            <div class="signal-row">
              <span>{{ t.receive }}</span>
              <div class="mini-meter vu-meter">
                <canvas ref="rxMeterCanvas" class="mini-meter-canvas" width="120" height="10"></canvas>
              </div>
              <strong>{{ uiActive ? rxLevelDb : "-∞ dB" }}</strong>
            </div>
            <div class="signal-row">
              <span>{{ t.transmit }}</span>
              <div class="mini-meter vu-meter">
                <canvas ref="txMeterCanvas" class="mini-meter-canvas" width="120" height="10"></canvas>
              </div>
              <strong>{{ uiActive ? txLevelDb : "-∞ dB" }}</strong>
            </div>
          </div>
        </div>
        <div class="summary-item summary-serial" :title="t.serialData">
          <div class="serial-rate-stack">
            <div class="serial-rate-row">
              <span class="serial-led" :data-active="serialRxActive"></span>
              <span>{{ t.serialRx }}</span>
              <strong>{{ formatBitRate(serialRxBps) }}</strong>
            </div>
            <div class="serial-rate-row">
              <span class="serial-led" :data-active="serialTxActive"></span>
              <span>{{ t.serialTx }}</span>
              <strong>{{ formatBitRate(serialTxBps) }}</strong>
            </div>
          </div>
        </div>
      </div>
    </header>

    <!-- FMO 独立统计栏（与 NRL 顶栏同时显示，单独统计） -->
    <header class="topbar fmo-topbar">
      <div class="topbar-summary">
        <div class="summary-item summary-callsign">
          <span>FMO {{ t.localCallsign }}</span>
          <strong>{{ fmo.stats.callsign || "-" }}</strong>
        </div>
        <div class="summary-item">
          <span>{{ t.latency }}</span>
          <strong>{{ fmo.stats.latencyMs }} ms</strong>
        </div>
        <div class="summary-item">
          <span>{{ t.jitter }}</span>
          <strong>{{ fmo.stats.jitterMs }} ms</strong>
        </div>
        <div class="summary-item">
          <span>{{ t.loss }}</span>
          <strong>{{ fmo.stats.packetLoss.toFixed(1) }}%</strong>
        </div>
        <div class="summary-item">
          <span>{{ t.queue }}</span>
          <strong>{{ fmo.stats.queuedFrames }}</strong>
        </div>
        <div class="summary-item summary-signal fmo-vu">
          <div class="signal-stack">
            <div class="signal-row">
              <span>{{ t.receive }}</span>
              <div class="mini-meter vu-meter">
                <canvas ref="fmoRxMeterCanvas" class="mini-meter-canvas" width="120" height="10"></canvas>
              </div>
              <strong>{{ fmoRxLevelDb }}</strong>
            </div>
            <div class="signal-row">
              <span>{{ t.transmit }}</span>
              <div class="mini-meter vu-meter">
                <canvas ref="fmoTxMeterCanvas" class="mini-meter-canvas" width="120" height="10"></canvas>
              </div>
              <strong>{{ fmoTxLevelDb }}</strong>
            </div>
          </div>
        </div>
        <div class="summary-item">
          <span>{{ language === "zh" ? "收帧" : "RX Frames" }}</span>
          <strong>{{ fmo.stats.rxFrames }}</strong>
        </div>
        <div class="summary-item">
          <span>{{ language === "zh" ? "发帧" : "TX Frames" }}</span>
          <strong>{{ fmo.stats.txFrames }}</strong>
        </div>
        <div class="summary-item">
          <span>{{ language === "zh" ? "遥测" : "Tele" }}</span>
          <strong>{{ fmo.stats.serverInfo }}</strong>
        </div>
        <div class="summary-item">
          <span>{{ language === "zh" ? "文本" : "Text" }}</span>
          <strong>{{ fmo.stats.rxText }}</strong>
        </div>
      </div>
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
          <div class="callsign-stage-head">
            <div class="system-clock" aria-label="System time">
              <strong class="system-clock-time">{{ systemDateText }} · {{ systemClockText }}</strong>
            </div>
            <button
              class="icon-toggle block-icon-btn"
              :title="language === 'zh' ? '打开 PTT 悬浮窗' : 'Open floating PTT window'"
              @click="openPttWindow()"
            >
              <svg viewBox="0 0 24 24" aria-hidden="true">
                <path d="M14 4h6v6M20 4l-8 8M9 5H7a2 2 0 0 0-2 2v10a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2v-2" />
              </svg>
            </button>
          </div>
          <div class="callsign-duo">
            <div class="callsign-block callsign-nrl">
              <div class="callsign-block-tags">
                <button
                  class="callsign-block-tag clickable"
                  :class="{ online: nrlLinkState === 'online', stale: nrlLinkState === 'stale' }"
                  :disabled="runtime.busy"
                  :title="nrlLinkState !== 'off'
                    ? (language === 'zh' ? '点击断开' : 'Click to disconnect')
                    : (language === 'zh' ? '点击连接' : 'Click to connect')"
                  @click="
                    nrlLinkState !== 'off'
                      ? runtime.disconnect()
                      : runtime.connect()
                  "
                >NRL</button>
              </div>
              <div class="callsign-block-tools">
                <button
                  class="ghost-btn tool-pill nrl-ptt"
                  :class="{ 'ptt-active': nrlPttActive }"
                  :disabled="nrlPttDisabled"
                  @pointerdown.prevent="pressNrlPtt($event)"
                  @pointerup.prevent="releaseNrlPtt($event)"
                  @pointercancel.prevent="releaseNrlPtt($event)"
                >
                  PTT
                </button>
                <button
                  class="icon-toggle block-mute"
                  :class="{ active: !runtime.snapshot.isMonitoring }"
                  :disabled="runtime.busy"
                  :title="runtime.snapshot.isMonitoring ? t.enableMute : t.disableMute"
                  @click="runtime.toggleRx()"
                >
                  <svg viewBox="0 0 24 24" aria-hidden="true">
                    <path d="M4 14h3.5l4.5 4V6l-4.5 4H4z" />
                    <path
                      v-if="runtime.snapshot.isMonitoring"
                      d="M14.8 9.3a4.5 4.5 0 0 1 0 5.4m2.8-8.1a8.2 8.2 0 0 1 0 10.8"
                    />
                    <path v-else class="mute-slash" d="M3.2 3.2 20.8 20.8" />
                  </svg>
                </button>
              </div>
              <div class="callsign-display">
                <span class="callsign-text">
                  {{ currentTalker }}
                  <span v-if="nrlVoiceActive && runtime.snapshot.rxCodec" class="rx-codec-chip">
                    {{ runtime.snapshot.rxCodec }}
                  </span>
                </span>
              </div>
              <div class="callsign-meta">
                <span class="callsign-room callsign-region">{{ currentTalkerRegion }}</span>
                <span class="callsign-room">{{ runtime.config.serverName || "-" }}</span>
                <span class="callsign-room">{{ currentGroupText }}</span>
              </div>
              <div class="callsign-mini-spectrum" aria-hidden="true">
                <canvas
                  ref="nrlSpectrumCanvas"
                  class="callsign-mini-spectrum-canvas"
                  width="460"
                  height="64"
                ></canvas>
              </div>
            </div>
            <div class="callsign-bridge">
              <button
                class="bridge-btn"
                :class="{ active: runtime.snapshot.bridgeMode !== 0 }"
                :disabled="runtime.busy"
                :aria-label="bridgeModeText"
                :title="`${bridgeModeText} · ${t.bridgeTitle}`"
                @click="runtime.cycleBridge()"
              >
                <svg
                    v-if="runtime.snapshot.bridgeMode === 0"
                    class="bridge-off-icon"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="2"
                    stroke-linecap="round"
                    stroke-linejoin="round"
                    aria-hidden="true"
                  >
                    <path d="M2.5 12h19" />
                    <path d="M7 7l-4.5 5L7 17" />
                    <path d="M17 7l4.5 5L17 17" />
                    <path d="M4.5 19.5 19.5 4.5" />
                  </svg>
                  <template v-else><span
                    v-if="runtime.snapshot.bridgeMode & 1"
                    class="bridge-arrow"
                    :class="{ 'bridge-flash': runtime.snapshot.bridgeTxNrl }"
                    >←</span
                  ><span
                    v-if="runtime.snapshot.bridgeMode & 2"
                    class="bridge-arrow"
                    :class="{ 'bridge-flash': runtime.snapshot.bridgeTxFmo }"
                    >→</span
                  ></template>
              </button>
            </div>
            <div class="callsign-block callsign-fmo">
              <div class="callsign-block-tags">
                <button
                  class="callsign-block-tag clickable"
                  :class="{ online: fmo.state.mqttState === 'connected' }"
                  :disabled="fmo.busy"
                  :title="fmo.state.mqttState === 'connected'
                    ? (language === 'zh' ? '点击断开' : 'Click to disconnect')
                    : (language === 'zh' ? '点击连接' : 'Click to connect')"
                  @click="
                    fmo.state.mqttState === 'connected' ? fmo.disconnectMqtt() : connectFmoMqtt()
                  "
                >FMO</button>
                <button
                  class="callsign-block-tag clickable"
                  :class="{ online: fmoAprsOnline }"
                  :disabled="fmo.busy"
                  :title="fmo.state.aprsState !== 'disconnected'
                    ? (language === 'zh' ? '点击断开 APRS' : 'Click to disconnect APRS')
                    : (language === 'zh' ? '点击连接 APRS' : 'Click to connect APRS')"
                  @click="
                    fmo.state.aprsState !== 'disconnected' ? fmo.disconnectAprs() : connectFmoAprs()
                  "
                >APRS</button>
              </div>
              <div class="callsign-block-tools">
                <button
                  class="ghost-btn tool-pill fmo-ptt"
                  :class="{ 'ptt-active': fmoPttActive }"
                  :disabled="fmoPttDisabled"
                  @pointerdown.prevent="pressFmoPtt($event)"
                  @pointerup.prevent="releaseFmoPtt($event)"
                  @pointercancel.prevent="releaseFmoPtt($event)"
                >
                  PTT
                </button>
                <button
                  class="ghost-btn tool-pill fmo-qso"
                  :class="{ 'ptt-active': fmo.qso.phase !== 'idle' }"
                  :disabled="fmo.busy"
                  :title="language === 'zh' ? 'QSO 呼叫（APRS 信令）' : 'QSO call (APRS signaling)'"
                  @click="qsoDialogOpen = true"
                >
                  QSO
                </button>
                <button
                  class="ghost-btn tool-pill fmo-broadcast-btn"
                  :disabled="fmo.busy"
                  :title="language === 'zh' ? '立即广播我的服务器（APRS STATION）' : 'Broadcast my server now (APRS STATION)'"
                  @click="manualBroadcast"
                >
                  {{ language === "zh" ? "广播" : "BCAST" }}
                </button>
                <button
                  class="icon-toggle block-mute"
                  :class="{ active: fmoMuted }"
                  :disabled="fmo.busy"
                  :title="fmoMuted ? t.disableMute : t.enableMute"
                  @click="toggleFmoMute"
                >
                  <svg viewBox="0 0 24 24" aria-hidden="true">
                    <path d="M4 14h3.5l4.5 4V6l-4.5 4H4z" />
                    <path
                      v-if="!fmoMuted"
                      d="M14.8 9.3a4.5 4.5 0 0 1 0 5.4m2.8-8.1a8.2 8.2 0 0 1 0 10.8"
                    />
                    <path v-else class="mute-slash" d="M3.2 3.2 20.8 20.8" />
                  </svg>
                </button>
              </div>
              <div class="callsign-display callsign-display-fmo">
                <span class="callsign-text">
                  {{
                    fmo.stats.activeSpeaker
                      ? fmo.stats.activeSpeaker
                      : fmo.stats.callsign || "-"
                  }}
                  <span v-if="fmoVoiceActive && fmo.stats.rxCodec" class="rx-codec-chip">
                    {{ fmo.stats.rxCodec }}
                  </span>
                </span>
              </div>
              <!-- 说话人命中 APRS 用户表时，展示其信标附加信息（状态/频率/电台/天线/高度）；
                   位置行（网格/距离/方位）独立于用户表匹配，网格源也可显示 -->
              <div v-if="fmoSpeakerInfoLines.length || fmoSpeakerGeoLine" class="callsign-speaker-info">
                <span v-for="(line, i) in fmoSpeakerInfoLines" :key="i">{{ line }}</span>
                <span v-if="fmoSpeakerGeoLine" class="speaker-geo-line">{{ fmoSpeakerGeoLine }}</span>
              </div>
              <div class="callsign-meta">
                <span class="callsign-room">
                  {{ fmo.stats.callsign ? `${fmo.stats.callsign} · uid ${fmo.stats.uid || "-"}` : "-" }}
                </span>
                <span class="callsign-room">
                  {{ fmo.stats.mqttState === "connected" ? fmoMqttText : "MQTT " + fmoMqttText }}
                </span>
                <span class="callsign-room">
                  {{ fmo.stats.serverName || "-" }}
                  <template v-if="fmo.stats.serverHost">· {{ fmo.stats.serverHost }}:{{ fmo.stats.serverPort }}</template>
                </span>
              </div>
              <div class="callsign-mini-spectrum" aria-hidden="true">
                <canvas
                  ref="fmoSpectrumCanvas"
                  class="callsign-mini-spectrum-canvas"
                  width="460"
                  height="64"
                ></canvas>
              </div>
            </div>
          </div>
        </div>

        <div class="ops-grid ops-grid-3col">
          <!-- NRL 收藏服务器：常用服务器快捷切换 -->
          <section class="ops-panel">
            <div class="ops-head">
              <div>
                <p class="section-kicker">NRL {{ language === "zh" ? "收藏服务器" : "Favorite Servers" }}</p>
              </div>
              <div class="ops-head-right">
                <span
                  class="fmo-state-chip"
                  :data-state="runtime.snapshot.connection"
                >
                  {{ nrlStatusText }}
                </span>
              </div>
            </div>
            <div class="fmo-ops-body">
              <div v-if="nrlFavorites.length" class="fmo-server-list">
                <article
                  v-for="server in nrlFavorites"
                  :key="nrlFavoriteKey(server)"
                  class="fmo-server-row"
                  :class="{ selected: selectedNrlServerHost === normalizeHost(server.host) }"
                  @click="selectNrlServer(server)"
                >
                  <div class="fmo-server-main">
                    <strong>{{ server.name || server.host }}</strong>
                    <span>{{ server.host }}:{{ server.port }} · {{ server.online ?? "?" }}/{{ server.total ?? "?" }}</span>
                  </div>
                  <div class="fmo-server-actions">
                    <span
                      v-if="platform.loggedIn && normalizeHost(platform.authServerLabel) === normalizeHost(server.host)"
                      class="fmo-server-meta"
                    >{{ language === "zh" ? "已登录" : "Signed In" }}</span>
                    <button
                      class="icon-btn fmo-star-btn active"
                      :title="language === 'zh' ? '取消收藏' : 'Unfavorite'"
                      @click.stop="toggleNrlFavorite(server)"
                    >
                      <svg viewBox="0 0 24 24" width="14" height="14" fill="currentColor">
                        <path d="M12 17.3l-6.2 3.7 1.6-7-5.4-4.7 7.1-.6L12 2l2.9 6.7 7.1.6-5.4 4.7 1.6 7z"/>
                      </svg>
                    </button>
                  </div>
                </article>
              </div>
              <div v-else class="ops-empty">
                {{ language === "zh"
                  ? "暂无收藏，在右侧「服务器 → NRL服务器」列表点 ☆ 收藏"
                  : "No favorites yet. Star servers in Servers → NRL Servers." }}
              </div>
            </div>
          </section>

          <!-- FMO 收藏面板：点击切换并连接 -->
          <section class="ops-panel fmo-ops-panel">
            <div class="ops-head">
              <div>
                <p class="section-kicker">FMO {{ language === "zh" ? "服务器" : "Servers" }}</p>
              </div>
              <div class="ops-head-right">
                <span class="fmo-state-chip" :data-state="fmo.state.mqttState">
                  MQTT: {{ fmoMqttText }}
                </span>
                <span class="fmo-state-chip" :data-state="fmo.state.aprsState">
                  APRS: {{ fmoAprsText }}
                </span>
              </div>
            </div>
            <div class="fmo-ops-body">
              <div v-if="fmo.state.favorites.length" class="fmo-ops-list">
                <article
                  v-for="fav in fmo.state.favorites"
                  :key="fav.key"
                  class="fmo-server-row"
                  :class="{ selected: fmo.selectedServer()?.key === fav.key }"
                  @click="selectFmoServerAndConnect(fav as unknown as FmoServer)"
                >
                  <div class="fmo-server-main">
                    <strong>{{ fav.name || fav.callsign || fav.host }}</strong>
                    <span>{{ fav.host }}:{{ fav.port }} · {{ fav.online ?? "?" }}/{{ fav.total ?? "?" }}</span>
                  </div>
                  <div class="fmo-server-actions">
                    <span class="fmo-server-meta">★</span>
                    <button
                      class="icon-btn"
                      :title="language === 'zh' ? '查看详情' : 'Details'"
                      @click.stop="openFmoServerPopup(fav as unknown as FmoServer)"
                    >
                      <svg viewBox="0 0 24 24" width="14" height="14" fill="currentColor">
                        <path d="M12 2a10 10 0 1 0 0 20 10 10 0 0 0 0-20zm0 4.5a1.4 1.4 0 1 1 0 2.8 1.4 1.4 0 0 1 0-2.8zM10.8 11h2.4v7h-2.4z"/>
                      </svg>
                    </button>
                    <button
                      class="icon-btn"
                      :title="language === 'zh' ? '取消收藏' : 'Unfavorite'"
                      @click.stop="fmo.removeFavorite(fav.key)"
                    >
                      <svg viewBox="0 0 24 24" width="14" height="14" fill="currentColor">
                        <path d="M6 19V5h2v14H6zm4 0V5h2v14h-2zm4 0V5h2v14h-2z"/>
                      </svg>
                    </button>
                  </div>
                </article>
              </div>
              <div v-else class="ops-empty">
                {{ language === "zh"
                  ? "暂无收藏服务器，请连接 APRS 后在右侧「服务器」列表点 ☆ 收藏"
                  : "No favorites yet. Connect APRS, then star a server in the Servers tab." }}
              </div>
            </div>
          </section>

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
                <span
                  class="group-chip-count"
                  :title="language === 'zh' ? '点击查看在线设备' : 'Click to view online devices'"
                  @click.stop="showGroupDevices(group)"
                >{{ group.onlineDevNumber ?? 0 }}/{{ group.totalDevNumber ?? 0 }}</span>
              </button>
            </div>
          </section>
        </div>
      </article>

      <article class="card chat-card">
        <div class="section-head chat-head">
          <div class="chat-tabs">
            <button
              class="chat-tab"
              :class="{ active: chatTab === 'messages' }"
              @click="chatTab = 'messages'"
            >
              {{ t.commandText }}
            </button>
            <button
              class="chat-tab"
              :class="{ active: chatTab === 'logs' }"
              @click="chatTab = 'logs'"
            >
              {{ t.systemLogs }}
            </button>
            <button
              class="chat-tab"
              :class="{ active: chatTab === 'servers' }"
              @click="chatTab = 'servers'"
            >
              {{ language === "zh" ? "服务器" : "Servers" }}
            </button>
            <button
              class="chat-tab"
              :class="{ active: chatTab === 'users' }"
              @click="chatTab = 'users'"
            >
              {{ language === "zh" ? "用户" : "Users" }}
            </button>
            <button
              class="chat-tab"
              :class="{ active: chatTab === 'qso' }"
              @click="chatTab = 'qso'"
            >
              QSO
            </button>
          </div>
          <span class="chat-status">{{
            chatTab === "messages"
              ? t.messagesCount(chatMessages.length)
              : chatTab === "logs"
                ? t.messagesCount(runtime.timeline.length)
                : chatTab === "servers"
                  ? t.messagesCount(serverListTab === "nrl" ? platform.servers.length : fmo.state.servers.length)
                  : chatTab === "qso"
                    ? t.messagesCount(qsoSuccessList.length)
                    : t.messagesCount(fmo.state.clients.length)
          }}</span>
        </div>

        <template v-if="chatTab === 'messages'">
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
        </template>

        <!-- 滚动日志：与消息区 Tab 切换，占满卡片剩余高度，新日志贴底跟随 -->
        <div v-else-if="chatTab === 'logs'" ref="logListEl" class="log-list chat-log-list" @scroll.passive="handleLogScroll">
          <div v-if="runtime.timeline.length === 0" class="log-empty">
            {{ t.noLogs }}
          </div>
          <div
            v-for="item in logEntries"
            :key="item.entry.id"
            class="log-line"
            :data-tone="item.entry.tone"
            :title="`${item.entry.time} ${item.entry.title} ${item.entry.detail}`"
          >
            <span class="log-line-time" :style="item.colors && { color: item.colors.time }">
              {{ item.entry.time }}
            </span>
            <span class="log-line-title" :style="item.colors && { color: item.colors.title }">
              {{ item.entry.title }}
            </span>
            <span class="log-line-detail" :style="item.colors && { color: item.colors.detail }">
              {{ item.entry.detail }}
            </span>
          </div>
        </div>

        <!-- 服务器选择：NRL 全量列表 / FMO 发现列表 -->
        <div v-else-if="chatTab === 'servers'" class="fmo-tab-panel">
          <div class="auth-server-mode server-kind-switch">
            <button
              class="mode-chip"
              :data-active="serverListTab === 'nrl'"
              @click="serverListTab = 'nrl'"
            >
              {{ language === "zh" ? "NRL服务器" : "NRL Servers" }}
            </button>
            <button
              class="mode-chip"
              :data-active="serverListTab === 'fmo'"
              @click="serverListTab = 'fmo'"
            >
              {{ language === "zh" ? "FMO服务器" : "FMO Servers" }}
            </button>
          </div>

          <template v-if="serverListTab === 'nrl'">
            <input
              v-model="nrlServerSearch"
              class="group-search fmo-tab-filter"
              type="text"
              :placeholder="language === 'zh' ? '搜索服务器…' : 'Search servers…'"
            />
            <div class="log-list chat-log-list fmo-tab-list">
              <div v-if="!platform.servers.length" class="log-empty">
                {{ language === "zh" ? "暂无 NRL 服务器" : "No NRL servers." }}
              </div>
              <div v-else-if="!filteredNrlServers.length" class="log-empty">
                {{ language === "zh" ? "无匹配服务器" : "No matching servers" }}
              </div>
              <div v-else class="fmo-server-list">
                <article
                  v-for="server in filteredNrlServers"
                  :key="server.host"
                  class="fmo-server-row"
                  :class="{ selected: selectedNrlServerHost === normalizeHost(server.host) }"
                  @click="selectNrlServer(server)"
                >
                  <div class="fmo-server-main">
                    <strong>{{ server.name || server.host }}</strong>
                    <span>{{ server.host }}:{{ server.port }} · {{ server.online ?? "?" }}/{{ server.total ?? "?" }}</span>
                  </div>
                  <div class="fmo-server-actions">
                    <span
                      v-if="platform.loggedIn && normalizeHost(platform.authServerLabel) === normalizeHost(server.host)"
                      class="fmo-server-meta"
                    >{{ language === "zh" ? "已登录" : "Signed In" }}</span>
                    <button
                      class="icon-btn fmo-star-btn"
                      :class="{ active: isNrlFavorite(server) }"
                      :title="language === 'zh' ? '收藏' : 'Favorite'"
                      @click.stop="toggleNrlFavorite(server)"
                    >
                      <svg viewBox="0 0 24 24" width="14" height="14" fill="currentColor">
                        <path d="M12 17.3l-6.2 3.7 1.6-7-5.4-4.7 7.1-.6L12 2l2.9 6.7 7.1.6-5.4 4.7 1.6 7z"/>
                      </svg>
                    </button>
                  </div>
                </article>
              </div>
            </div>
            <div class="nrl-custom-server">
              <input v-model="customNrlServerHost" type="text" :placeholder="language === 'zh' ? '自定义服务器' : 'Custom host'" />
              <input v-model="customNrlServerPort" type="text" inputmode="numeric" placeholder="60050" />
              <button class="ghost-btn compact" @click="applyCustomNrlServer">{{ language === "zh" ? "使用" : "Use" }}</button>
            </div>
            <div v-if="nrlServerError" class="auth-error">{{ nrlServerError }}</div>
          </template>

          <template v-else>
          <input
            v-model="fmoServerFilter"
            class="group-search fmo-tab-filter"
            type="text"
            :placeholder="language === 'zh' ? '搜索名称/呼号/主机…' : 'Search name/callsign/host…'"
          />
          <div class="log-list chat-log-list fmo-tab-list">
          <div v-if="!fmo.state.servers.length && !fmo.state.favorites.length" class="log-empty">
            {{ language === "zh"
              ? "暂无服务器，请连接 APRS 发现服务器"
              : "No servers yet. Connect APRS to discover servers." }}
          </div>
          <div v-else-if="!filteredFmoServers.length && !fmo.state.favorites.length" class="log-empty">
            {{ language === "zh" ? "无匹配服务器" : "No matching servers" }}
          </div>
          <div class="fmo-server-list" v-if="filteredFmoServers.length">
            <article
              v-for="s in filteredFmoServers"
              :key="s.key"
              class="fmo-server-row"
              :class="{ selected: fmo.selectedServer()?.key === s.key }"
              @click="selectFmoServerAndConnect(s)"
            >
              <div class="fmo-server-main">
                <strong>{{ s.name || s.callsign }}</strong>
                <span>{{ s.host }}:{{ s.port }}</span>
              </div>
              <div class="fmo-server-actions">
                <span
                  class="fmo-server-meta online-count"
                  :class="{ 'has-online': (s.online ?? 0) > 0 }"
                >
                  {{ language === "zh" ? "在线" : "Online" }} {{ s.online ?? 0 }}/{{ s.total ?? "?" }}
                </span>
                <span v-if="s.cover_km" class="fmo-server-meta">{{ s.cover_km }}km</span>
                <button
                  class="icon-btn"
                  :title="language === 'zh' ? '查看详情' : 'Details'"
                  @click.stop="openFmoServerPopup(s)"
                >
                  <svg viewBox="0 0 24 24" width="14" height="14" fill="currentColor">
                    <path d="M12 2a10 10 0 1 0 0 20 10 10 0 0 0 0-20zm0 4.5a1.4 1.4 0 1 1 0 2.8 1.4 1.4 0 0 1 0-2.8zM10.8 11h2.4v7h-2.4z"/>
                  </svg>
                </button>
                <button
                  class="icon-btn fmo-star-btn"
                  :class="{ active: isFmoServerFavorited(s) }"
                  :title="language === 'zh' ? '收藏' : 'Favorite'"
                  @click.stop="toggleFmoServerFavorite(s)"
                >
                  <svg viewBox="0 0 24 24" width="14" height="14" fill="currentColor">
                    <path d="M12 17.3l-6.2 3.7 1.6-7-5.4-4.7 7.1-.6L12 2l2.9 6.7 7.1.6-5.4 4.7 1.6 7z"/>
                  </svg>
                </button>
              </div>
              <div v-if="fmoServerDetailLine(s)" class="fmo-user-recent">
                <span>{{ fmoServerDetailLine(s) }}</span>
              </div>
            </article>
          </div>
          <div class="fmo-favorites" v-if="fmo.state.favorites.length">
            <article
              v-for="fav in fmo.state.favorites"
              :key="fav.key"
              class="fmo-server-row"
              :class="{ selected: fmo.selectedServer()?.key === fav.key }"
              @click="selectFmoServerAndConnect(fav as unknown as FmoServer)"
            >
              <div class="fmo-server-main">
                <strong>{{ fav.name || fav.callsign || fav.host }}</strong>
                <span>{{ fav.host }}:{{ fav.port }}</span>
              </div>
              <div class="fmo-server-actions">
                <span class="fmo-server-meta">★</span>
                <button
                  class="icon-btn"
                  :title="language === 'zh' ? '查看详情' : 'Details'"
                  @click.stop="openFmoServerPopup(fav as unknown as FmoServer)"
                >
                  <svg viewBox="0 0 24 24" width="14" height="14" fill="currentColor">
                    <path d="M12 2a10 10 0 1 0 0 20 10 10 0 0 0 0-20zm0 4.5a1.4 1.4 0 1 1 0 2.8 1.4 1.4 0 0 1 0-2.8zM10.8 11h2.4v7h-2.4z"/>
                  </svg>
                </button>
                <button class="icon-btn" @click.stop="fmo.removeFavorite(fav.key)">
                  <svg viewBox="0 0 24 24" width="14" height="14" fill="currentColor">
                    <path d="M6 19V5h2v14H6zm4 0V5h2v14h-2zm4 0V5h2v14h-2z"/>
                  </svg>
                </button>
              </div>
            </article>
          </div>
          </div>
          </template>
        </div>

        <!-- FMO 用户列表：APRS 客户端信标（呼号/UID/状态/最后出现时间） -->
        <div v-else-if="chatTab === 'users'" class="fmo-tab-panel">
          <input
            v-model="fmoUserFilter"
            class="group-search fmo-tab-filter"
            type="text"
            :placeholder="language === 'zh' ? '搜索呼号/状态/uid…' : 'Search callsign/status/uid…'"
          />
          <div class="log-list chat-log-list fmo-tab-list">
          <div v-if="!fmo.state.clients.length" class="log-empty">
            {{ language === "zh"
              ? "暂无用户信标，连接 APRS 后自动收集"
              : "No client beacons yet. They appear after APRS connects." }}
          </div>
          <div v-else-if="!filteredFmoClients.length" class="log-empty">
            {{ language === "zh" ? "无匹配用户" : "No matching users" }}
          </div>
          <article
            v-for="c in filteredFmoClients"
            :key="c.callsign"
            class="fmo-user-row"
            :title="language === 'zh' ? '点击查看详情' : 'Click for details'"
            @click="fmoUserPopup = c"
          >
            <div class="fmo-server-main">
              <strong>{{ c.callsign }}</strong>
              <span>{{
                c.status_text || c.comment || (c.freq ? c.freq.toFixed(4) + " MHz" : "") || c.kind || ""
              }}</span>
            </div>
            <div class="fmo-server-actions">
              <span v-if="c.uid" class="fmo-server-meta">uid {{ c.uid }}</span>
              <span class="fmo-server-meta">{{ fmtClientTime(c.last_seen) }}</span>
            </div>
            <div v-if="fmoClientDetailLine(c)" class="fmo-user-recent">
              <span>{{ fmoClientDetailLine(c) }}</span>
            </div>
            <div v-if="fmoClientRecentExtras(c).length" class="fmo-user-recent">
              <span v-for="(m, i) in fmoClientRecentExtras(c)" :key="i">{{ fmtClientTime(m.ts) }} {{ m.text }}</span>
            </div>
          </article>
          </div>
        </div>

        <!-- QSO 成功通联列表（点击可再次呼叫） -->
        <div v-else class="fmo-tab-panel">
          <div class="log-list chat-log-list fmo-tab-list">
          <div v-if="!qsoSuccessList.length" class="log-empty">
            {{ language === "zh"
              ? "暂无成功通联（QSO 呼叫接通后记录在这里）"
              : "No successful QSOs yet. Established calls are listed here." }}
          </div>
          <article
            v-for="(r, i) in qsoSuccessList"
            :key="i"
            class="fmo-user-row"
            :title="language === 'zh' ? '点击再次呼叫' : 'Click to call again'"
            @click="qsoCallAgain(r)"
          >
            <div class="fmo-server-main">
              <strong>{{ r.dir === "out" ? "→" : "←" }} {{ r.peer }}</strong>
              <span>{{ r.result }}</span>
            </div>
            <div v-if="r.comment" class="fmo-user-recent">
              <span>{{ language === "zh" ? "祝福" : "Wish" }}: {{ r.comment }}</span>
            </div>
            <div class="fmo-server-actions">
              <span v-if="r.grid" class="fmo-server-meta">{{ r.grid }}</span>
              <span v-if="r.peer_uid" class="fmo-server-meta">uid {{ r.peer_uid }}</span>
              <span class="fmo-server-meta">{{ fmtClientDateTime(r.ts) }}</span>
            </div>
          </article>
          </div>
        </div>
      </article>
    </section>

    <transition name="drawer-fade">
      <div v-if="showSettings" class="drawer-backdrop" @click="showSettings = false"></div>
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

      <!-- 设置标签页：NRL / FMO 分开 -->
      <div class="settings-tabs">
        <button
          class="settings-tab"
          :class="{ active: settingsTab === 'nrl' }"
          @click="settingsTab = 'nrl'"
        >
          NRL
        </button>
        <button
          class="settings-tab"
          :class="{ active: settingsTab === 'fmo' }"
          @click="settingsTab = 'fmo'"
        >
          FMO
        </button>
      </div>

      <!-- NRL 设置 -->
      <div v-if="settingsTab === 'nrl'" class="settings-list">
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
          <span>{{ language === "zh" ? "语音编码" : "Voice Codec" }}</span>
          <div class="auth-server-mode">
            <button
              class="mode-chip"
              :data-active="runtime.config.voiceCodec !== 'opus'"
              @click="runtime.saveConfig({ ...runtime.config, voiceCodec: 'alaw' })"
            >
              G.711
            </button>
            <button
              class="mode-chip"
              :data-active="runtime.config.voiceCodec === 'opus'"
              @click="runtime.saveConfig({ ...runtime.config, voiceCodec: 'opus' })"
            >
              Opus
            </button>
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
        <div class="serial-tunnel-box">
          <div class="serial-tunnel-head">
            <div>
              <span>{{ t.serialTunnel }}</span>
              <strong>{{ serialStatusText }}</strong>
            </div>
          </div>
          <div class="setting-form serial-form">
            <div class="serial-port-row">
              <label class="serial-port-field" :title="t.serialPort">
                <span>{{ t.serialPort }}</span>
                <select
                  v-model="serialTunnelDraft.portName"
                  @focus="refreshSerialPortsUi"
                  @pointerdown="refreshSerialPortsUi"
                  @change="saveNetworkConfig"
                >
                  <option value=""></option>
                  <option v-for="port in serialPortOptions" :key="port" :value="port">
                    {{ port }}
                  </option>
                </select>
              </label>
              <button
                class="ghost-btn compact serial-start-btn"
                :disabled="runtime.busy || !runtime.serialTunnel.supported || !serialTunnelDraft.portName"
                @click="runtime.serialTunnel.running ? stopSerialTunnelUi() : startSerialTunnelUi()"
              >
                {{ runtime.serialTunnel.running ? t.stopSerial : t.startSerial }}
              </button>
            </div>
            <label>
              <span>{{ t.baudRate }}</span>
              <select v-model.number="serialTunnelDraft.baudRate" @change="saveNetworkConfig">
                <option :value="9600">9600</option>
                <option :value="19200">19200</option>
                <option :value="38400">38400</option>
                <option :value="57600">57600</option>
                <option :value="115200">115200</option>
                <option :value="230400">230400</option>
              </select>
            </label>
            <label>
              <span>{{ t.dataBits }}</span>
              <select v-model.number="serialTunnelDraft.dataBits" @change="saveNetworkConfig">
                <option :value="7">7</option>
                <option :value="8">8</option>
              </select>
            </label>
            <label>
              <span>{{ t.parity }}</span>
              <select v-model="serialTunnelDraft.parity" @change="saveNetworkConfig">
                <option value="none">{{ t.parityNone }}</option>
                <option value="odd">{{ t.parityOdd }}</option>
                <option value="even">{{ t.parityEven }}</option>
              </select>
            </label>
            <label>
              <span>{{ t.stopBits }}</span>
              <select v-model="serialTunnelDraft.stopBits" @change="saveNetworkConfig">
                <option value="one">{{ t.stopOne }}</option>
                <option value="two">{{ t.stopTwo }}</option>
              </select>
            </label>
            <label>
              <span>{{ t.flowControl }}</span>
              <select v-model="serialTunnelDraft.flowControl" @change="saveNetworkConfig">
                <option value="none">{{ t.flowNone }}</option>
                <option value="software">{{ t.flowSoftware }}</option>
                <option value="hardware">{{ t.flowHardware }}</option>
              </select>
            </label>
          </div>
          <div class="setting-row serial-stats-row">
            <span>{{ t.serialStats }}</span>
            <strong>{{ formatBytes(runtime.serialTunnel.rxBytes) }} / {{ formatBytes(runtime.serialTunnel.txBytes) }}</strong>
          </div>
        </div>
      </div>

      <!-- FMO 设置 -->
      <div v-if="settingsTab === 'fmo'" class="settings-list">
        <div class="fmo-panel">
          <!-- ① 身份 -->
          <div class="fmo-section">
            <div class="fmo-section-head">
              <span class="fmo-section-tag">①</span>
              <span>{{ language === "zh" ? "身份" : "Identity" }}</span>
            </div>
            <div class="fmo-id-grid">
              <div class="fmo-id-cell">
                <span>{{ language === "zh" ? "呼号" : "Callsign" }}</span>
                <strong>{{ fmo.state.identity.callsign || "-" }}</strong>
              </div>
              <div class="fmo-id-cell">
                <span>UID</span>
                <strong>{{ fmo.state.identity.uid || "-" }}</strong>
              </div>
              <div class="fmo-id-cell">
                <span>APRS Passcode</span>
                <strong>{{ fmo.state.passcode || "-" }}</strong>
              </div>
            </div>
          </div>

          <!-- ② 证书（4 个独立导入） -->
          <div class="fmo-section">
            <div class="fmo-section-head">
              <span class="fmo-section-tag">②</span>
              <span>{{ language === "zh" ? "证书（需完整 4 个）" : "Certs (all 4)" }}</span>
              <span class="fmo-cert-count">{{ fmoCertReadyCount }}/4</span>
            </div>
            <div class="fmo-cert-grid">
              <button
                v-for="slot in fmoCertSlots"
                :key="slot.name"
                class="fmo-cert-slot"
                :data-ready="fmo.state.certs.some((c) => c.name === slot.name)"
                :disabled="fmo.busy"
                :title="language === 'zh' ? `选择 ${slot.file}` : `Select ${slot.file}`"
                @click="onFmoCertSlotChange(slot.name)"
              >
                <span class="fmo-cert-slot-name">{{ slot.label }}</span>
                <span class="fmo-cert-slot-file">{{ slot.file }}</span>
                <small v-if="fmo.state.certs.some((c) => c.name === slot.name)" class="fmo-cert-slot-ok">
                  ✓ {{ language === "zh" ? "已导入" : "Imported" }}
                </small>
                <small v-else class="fmo-cert-slot-choose">
                  {{ language === "zh" ? "点击选择文件…" : "Click to choose…" }}
                </small>
              </button>
            </div>
            <small v-if="fmoCertMsg" class="fmo-cert-msg">{{ fmoCertMsg }}</small>
          </div>

          <!-- ③ 自动获取证书（绑定 MAC 激活） -->
          <div class="fmo-section">
            <div class="fmo-section-head">
              <span class="fmo-section-tag">③</span>
              <span>{{ language === "zh" ? "自动获取证书" : "Auto Activate" }}</span>
            </div>
            <div class="fmo-conn-row">
              <span class="fmo-conn-label">{{ language === "zh" ? "本机 MAC" : "Device MAC" }}</span>
              <span class="fmo-conn-value">{{ fmoActivateMac || "-" }}</span>
            </div>
            <div class="fmo-conn-row">
              <span class="fmo-conn-label">{{ language === "zh" ? "证书服务器" : "Cert Server" }}</span>
              <input
                v-model="fmoActivateServer"
                type="text"
                class="text-input"
                placeholder="www.hamptt.com"
              />
              <button class="ghost-btn compact" :disabled="fmo.busy" @click="saveFmoActivateServer">
                {{ language === "zh" ? "保存" : "Save" }}
              </button>
            </div>
            <div class="fmo-conn-row fmo-conn-row--solo">
              <button
                class="ghost-btn compact"
                :disabled="fmo.busy || fmoActivating"
                @click="runFmoActivate"
              >
                {{
                  fmoActivating
                    ? language === "zh" ? "获取中…" : "Activating…"
                    : language === "zh" ? "自动获取证书" : "Fetch Certificate"
                }}
              </button>
            </div>
            <small v-if="fmoActivateMsg" class="fmo-cert-msg">{{ fmoActivateMsg }}</small>
            <small class="fmo-cert-msg">
              {{
                language === "zh"
                  ? "前提：本机 MAC 需已在 hamptt.com 登记并绑定用户，否则返回未登记/未绑定"
                  : "Requires this MAC registered & bound on hamptt.com first"
              }}
            </small>
          </div>

          <!-- ④ 连接 -->
          <div class="fmo-section">
            <div class="fmo-section-head">
              <span class="fmo-section-tag">④</span>
              <span>{{ language === "zh" ? "连接" : "Connection" }}</span>
            </div>
            <div class="fmo-conn-row">
              <span class="fmo-conn-label">APRS</span>
              <span class="fmo-conn-value" :data-state="fmo.state.aprsState">
                {{ fmoAprsText }}
              </span>
              <button
                class="ghost-btn compact"
                :disabled="fmo.busy"
                @click="fmo.state.aprsState !== 'disconnected' ? fmo.disconnectAprs() : connectFmoAprs()"
              >
                {{
                  fmo.state.aprsState !== "disconnected"
                    ? language === "zh" ? "断开" : "Disconnect"
                    : language === "zh" ? "连接" : "Connect"
                }}
              </button>
            </div>
            <div class="fmo-conn-row">
              <span class="fmo-conn-label">MQTT</span>
              <span class="fmo-conn-value" :data-state="fmo.state.mqttState">
                {{ fmoMqttText }}
              </span>
              <button
                class="ghost-btn compact"
                :disabled="fmo.busy"
                @click="fmo.state.mqttState === 'connected' ? fmo.disconnectMqtt() : connectFmoMqtt()"
              >
                {{
                  fmo.state.mqttState === "connected"
                    ? language === "zh" ? "FMO 断开" : "FMO Disconnect"
                    : language === "zh" ? "FMO 连接" : "FMO Connect"
                }}
              </button>
            </div>
            <div class="fmo-conn-detail" v-if="fmo.state.mqttClientId">
              MQTT Client ID: {{ fmo.state.mqttClientId }}
            </div>
            <label class="fmo-conn-row" title="关闭后服务器会返回本客户端发布的消息，用于调试">
              <span class="fmo-conn-label">MQTT No Local</span>
              <input type="checkbox" :checked="fmo.state.mqttNoLocal" @change="toggleFmoNoLocal">
            </label>
            <div class="fmo-conn-detail" v-if="fmo.state.mqttDetail || fmo.state.aprsDetail">
              {{ fmo.state.mqttDetail || fmo.state.aprsDetail }}
            </div>
          </div>

          <!-- ⑤ QSO（自动接受 + 记录） -->
          <div class="fmo-section">
            <div class="fmo-section-head">
              <span class="fmo-section-tag">⑤</span>
              <span>QSO</span>
            </div>
            <div class="fmo-conn-row">
              <span class="fmo-conn-label">{{ language === "zh" ? "来电处理" : "Incoming call" }}</span>
              <div class="auth-server-mode">
                <button
                  class="mode-chip"
                  :data-active="!fmo.qso.autoAccept"
                  @click="fmo.setQsoAutoAccept(false)"
                >
                  {{ language === "zh" ? "弹窗确认" : "Ask" }}
                </button>
                <button
                  class="mode-chip"
                  :data-active="fmo.qso.autoAccept"
                  @click="fmo.setQsoAutoAccept(true)"
                >
                  {{ language === "zh" ? "自动接受" : "Auto-accept" }}
                </button>
              </div>
            </div>
            <div class="fmo-qso-log" v-if="qsoLogRecent.length">
              <div v-for="(r, i) in qsoLogRecent" :key="i" class="fmo-qso-log-row">
                <span>{{ fmtQsoTime(r.ts) }}</span>
                <span>{{ r.dir === "out" ? "→" : "←" }} {{ r.peer }}</span>
                <span>{{ r.result }}</span>
                <span v-if="r.comment">{{ r.comment }}</span>
              </div>
            </div>
            <small v-else class="fmo-cert-msg">
              {{ language === "zh" ? "暂无 QSO 记录（保存在数据目录 qso_log.json）" : "No QSO records yet (saved to qso_log.json)" }}
            </small>
          </div>

          <!-- ⑥ 服务器广播 -->
          <div class="fmo-section">
            <div class="fmo-section-head">
              <span class="fmo-section-tag">⑥</span>
              <span>{{ language === "zh" ? "服务器广播" : "Server Broadcast" }}</span>
            </div>
            <div class="fmo-bc-form">
              <label>
                <span>{{ language === "zh" ? "服务器名称" : "Name" }}</span>
                <input v-model="broadcastDraft.name" type="text" class="text-input" maxlength="32" :placeholder="language === 'zh' ? '我的 FMO 服务器（最大 32 字符）' : 'My FMO server (max 32 chars)'" />
              </label>
              <label>
                <span>{{ language === "zh" ? "地址" : "Host" }}</span>
                <input v-model="broadcastDraft.host" type="text" class="text-input" placeholder="fmo.example.com" />
              </label>
              <button class="ghost-btn fmo-bc-fill" :disabled="!fmo.selectedServer()" @click="fillBroadcastFromServer">
                {{ language === "zh" ? "带入当前服务器地址" : "Use selected server" }}
              </button>
              <div class="fmo-bc-grid">
                <label>
                  <span>{{ language === "zh" ? "端口" : "Port" }}</span>
                  <input v-model.number="broadcastDraft.port" type="number" class="text-input" placeholder="1883" />
                </label>
                <label>
                  <span>{{ language === "zh" ? "国家码" : "Country" }}</span>
                  <input v-model="broadcastDraft.country" type="text" class="text-input" placeholder="CN" />
                </label>
                <label>
                  <span>{{ language === "zh" ? "SSID (0-15)" : "SSID (0-15)" }}</span>
                  <input v-model.number="broadcastDraft.ssid" type="number" min="0" max="15" class="text-input" placeholder="0" />
                </label>
                <label>
                  <span>{{ language === "zh" ? "覆盖 (km)" : "Coverage (km)" }}</span>
                  <input v-model.number="broadcastDraft.cover_km" type="number" class="text-input" placeholder="100" />
                </label>
                <label>
                  <span>{{ language === "zh" ? "在线" : "Online" }}</span>
                  <input v-model.number="broadcastDraft.online" type="number" class="text-input" placeholder="0" />
                </label>
                <label>
                  <span>{{ language === "zh" ? "峰值" : "Peak" }}</span>
                  <input v-model.number="broadcastDraft.peak" type="number" class="text-input" placeholder="0" />
                </label>
                <label>
                  <span>{{ language === "zh" ? "纬度" : "Lat" }}</span>
                  <input v-model.number="broadcastDraft.lat" type="number" step="0.0001" class="text-input" placeholder="39.9" />
                </label>
                <label>
                  <span>{{ language === "zh" ? "经度" : "Lon" }}</span>
                  <input v-model.number="broadcastDraft.lon" type="number" step="0.0001" class="text-input" placeholder="116.4" />
                </label>
              </div>
              <small class="fmo-cert-msg">
                {{ language === "zh"
                  ? `当前自动值：在线 ${fmo.stats.presenceOnline} / 峰值 ${fmo.stats.presencePeak}（填 0 使用自动）`
                  : `Auto: online ${fmo.stats.presenceOnline} / peak ${fmo.stats.presencePeak} (set 0 to use auto)` }}
              </small>
            </div>
            <div class="fmo-conn-row">
              <span class="fmo-conn-label">{{ language === "zh" ? "自动广播" : "Auto" }}</span>
              <div class="auth-server-mode fmo-bc-modes">
                <button class="mode-chip" :data-active="broadcastDraft.mode_min === 0" @click="broadcastDraft.mode_min = 0">
                  {{ language === "zh" ? "关闭" : "Off" }}
                </button>
                <button class="mode-chip" :data-active="broadcastDraft.mode_min === 5" :disabled="!fmo.canBroadcast()" @click="broadcastDraft.mode_min = 5">5min</button>
                <button class="mode-chip" :data-active="broadcastDraft.mode_min === 10" :disabled="!fmo.canBroadcast()" @click="broadcastDraft.mode_min = 10">10min</button>
                <button class="mode-chip" :data-active="broadcastDraft.mode_min === 60" :disabled="!fmo.canBroadcast()" @click="broadcastDraft.mode_min = 60">60min</button>
              </div>
            </div>
            <small v-if="!fmo.canBroadcast()" class="fmo-cert-msg">
              {{ language === "zh" ? "需以 super 身份连接自己的服务器才能开启/执行广播" : "Broadcast requires connecting to your own server as super" }}
            </small>
            <div class="auth-actions">
              <button class="ghost-btn" :disabled="fmo.busy || !fmo.canBroadcast()" @click="manualBroadcast">
                {{ language === "zh" ? "立即广播" : "Broadcast now" }}
              </button>
              <button class="primary-btn" :disabled="fmo.busy" @click="saveBroadcastConfig">
                {{ language === "zh" ? "保存" : "Save" }}
              </button>
            </div>
          </div>

          <!-- ⑦ 个人信标（BEACON） -->
          <div class="fmo-section">
            <div class="fmo-section-head">
              <span class="fmo-section-tag">⑦</span>
              <span>{{ language === "zh" ? "个人信标（BEACON）" : "Personal Beacon (BEACON)" }}</span>
            </div>
            <div class="fmo-bc-form">
              <div class="fmo-bc-grid">
                <label>
                  <span>{{ language === "zh" ? "SSID (0-15)" : "SSID (0-15)" }}</span>
                  <input v-model.number="beaconDraft.ssid" type="number" min="0" max="15" class="text-input" placeholder="0" />
                </label>
                <label>
                  <span>{{ language === "zh" ? "直频频率 (MHz)" : "Freq (MHz)" }}</span>
                  <input v-model.number="beaconDraft.freq_mhz" type="number" step="0.0001" class="text-input" placeholder="431.0000" />
                </label>
                <label>
                  <span>{{ language === "zh" ? "天线高度 (m)" : "Height (m)" }}</span>
                  <input v-model.number="beaconDraft.height_m" type="number" min="0" class="text-input" placeholder="0" />
                </label>
                <label>
                  <span>{{ language === "zh" ? "电台名称" : "Rig" }}</span>
                  <input v-model="beaconDraft.rig" type="text" class="text-input" maxlength="16" :placeholder="language === 'zh' ? '海能达PDC580（≤16 字符）' : 'Rig model (max 16 chars)'" />
                </label>
                <label>
                  <span>{{ language === "zh" ? "天线型号" : "Antenna" }}</span>
                  <input v-model="beaconDraft.ant" type="text" class="text-input" maxlength="16" :placeholder="language === 'zh' ? 'QTH江苏靖江（≤16 字符）' : 'Antenna (max 16 chars)'" />
                </label>
              </div>
              <label>
                <span>{{ language === "zh" ? "APRS 个性化消息" : "APRS message" }}</span>
                <input v-model="beaconDraft.aprs_msg" type="text" class="text-input" maxlength="64" :placeholder="language === 'zh' ? '信标成功后以 APFMO2 跟发（≤64 字符，留空不发）' : 'Sent as APFMO2 after beacon (max 64 chars, empty = off)'" />
              </label>
              <label>
                <span>{{ language === "zh" ? "登录公告" : "Login notice" }}</span>
                <input v-model="beaconDraft.notice" type="text" class="text-input" maxlength="128" :placeholder="language === 'zh' ? '服务器广播成功后以 APFMO1 跟发（≤128 字符，留空不发）' : 'Sent as APFMO1 after station broadcast (max 128 chars, empty = off)'" />
              </label>
              <label>
                <span>{{ language === "zh" ? "QSO 祝福" : "QSO greeting" }}</span>
                <input v-model="beaconDraft.qso_msg" type="text" class="text-input" maxlength="128" :placeholder="language === 'zh' ? '仅存储暂不发送（传输机制待实网研究）' : 'Stored only, not sent yet (mechanism TBD)'" />
              </label>
              <small class="fmo-cert-msg">
                {{ language === "zh"
                  ? "信标位置使用上方广播配置的经纬度；发送需 APRS 已验证登录且频率 > 0（无需 super）。高度填 0 则报文省略 HEIGHT 段。"
                  : "Position reuses broadcast lat/lon above; sending needs verified APRS login and freq > 0 (no super required). Height 0 omits HEIGHT." }}
              </small>
            </div>
            <div class="fmo-conn-row">
              <span class="fmo-conn-label">{{ language === "zh" ? "周期信标" : "Auto" }}</span>
              <div class="auth-server-mode fmo-bc-modes">
                <button class="mode-chip" :data-active="!beaconDraft.enabled" @click="beaconDraft.enabled = false">
                  {{ language === "zh" ? "关闭" : "Off" }}
                </button>
                <button class="mode-chip" :data-active="beaconDraft.enabled" @click="beaconDraft.enabled = true">
                  {{ language === "zh" ? "开启（10 分钟）" : "On (10 min)" }}
                </button>
              </div>
            </div>
            <small class="fmo-cert-msg">
              {{ language === "zh"
                ? `当前：${fmo.stats.beaconEnabled ? "已开启" : "未开启"}${fmo.stats.beaconLastSent ? "，上次发送 " + new Date(fmo.stats.beaconLastSent * 1000).toLocaleTimeString() : ""}`
                : `Status: ${fmo.stats.beaconEnabled ? "enabled" : "disabled"}${fmo.stats.beaconLastSent ? ", last sent " + new Date(fmo.stats.beaconLastSent * 1000).toLocaleTimeString() : ""}` }}
            </small>
            <div class="auth-actions">
              <button class="ghost-btn" :disabled="fmo.busy" @click="manualBeacon">
                {{ language === "zh" ? "立即发送" : "Send now" }}
              </button>
              <button class="primary-btn" :disabled="fmo.busy" @click="saveBeaconConfig">
                {{ language === "zh" ? "保存" : "Save" }}
              </button>
            </div>
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

    <aside class="auth-drawer" :data-open="showLogin">
      <div class="drawer-head">
        <div>
          <h2>{{ t.serverLogin }}</h2>
        </div>
        <button class="ghost-btn compact-ghost" @click="showLogin = false">{{ t.close }}</button>
      </div>

      <div class="settings-list">
        <div class="setting-form auth-form">
          <div class="auth-switch">
            <button
              class="auth-switch-btn"
              :data-active="!showRegister && !showTokenLogin"
              @click="backToLoginForm"
            >
              {{ t.loginPlatformAction }}
            </button>
            <button
              class="auth-switch-btn"
              :data-active="showTokenLogin"
              @click="openTokenLoginForm"
            >
              {{ t.tokenLoginAction }}
            </button>
            <button
              class="auth-switch-btn"
              :data-active="showRegister"
              @click="openRegisterForm"
            >
              {{ t.openRegister }}
            </button>
          </div>
          <div class="auth-server-mode">
            <button
              class="mode-chip"
              :data-active="!platform.useCustomAuthServer"
              @click="platform.useCustomAuthServer = false"
            >
              {{ t.serverModeList }}
            </button>
            <button
              class="mode-chip"
              :data-active="platform.useCustomAuthServer"
              @click="platform.useCustomAuthServer = true"
            >
              {{ t.serverModeCustom }}
            </button>
          </div>
          <template v-if="!platform.useCustomAuthServer">
            <div class="login-server-row">
              <label class="login-server">
                <span>{{ t.authServer }}</span>
                <select v-model="platform.authServerHost">
                  <option v-for="server in platform.servers" :key="server.host" :value="server.host">
                    {{ server.name }} · {{ server.host }}
                  </option>
                </select>
              </label>
              <button class="icon-btn" :disabled="platform.busy || registerBusy" :title="t.refreshServers" @click="platform.refreshServers()">
                <svg viewBox="0 0 24 24" aria-hidden="true">
                  <path d="M4 12a8 8 0 0 1 14.9-5.3L21 9v6h-6l2.4-2.4A10 10 0 0 0 4.3 11H2v4h4.3A8 8 0 0 1 4 12z"/>
                </svg>
              </button>
            </div>
          </template>
          <label v-else class="full-width">
            <span>{{ t.authServerCustom }}</span>
            <input v-model="platform.customAuthServerHost" type="text" :placeholder="t.customServerPlaceholder" />
          </label>

          <template v-if="!showRegister && !showTokenLogin">
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
            <div class="auth-actions">
              <button class="ghost-btn" :disabled="platform.busy" @click="openRegisterForm">
                {{ t.openRegister }}
              </button>
              <button class="primary-btn" :disabled="platform.busy" @click="loginPlatform">
                {{ platform.busy ? t.loggingIn : platform.loggedIn ? t.relogin : t.loginPlatformAction }}
              </button>
            </div>
          </template>
          <template v-else-if="showTokenLogin">
            <label class="full-width">
              <span>{{ t.hamidToken }}</span>
              <input
                v-model="platform.hamidToken"
                type="password"
                autocomplete="off"
                spellcheck="false"
                :placeholder="t.hamidTokenPlaceholder"
                @keydown.enter.prevent="loginPlatformWithToken"
              />
            </label>
            <div class="auth-tip">{{ t.hamidTokenTip }}</div>
            <div class="auth-actions">
              <button class="ghost-btn" :disabled="platform.busy" @click="backToLoginForm">
                {{ t.backToLogin }}
              </button>
              <button class="primary-btn" :disabled="platform.busy" @click="loginPlatformWithToken">
                {{ platform.busy ? t.loggingIn : platform.loggedIn ? t.relogin : t.tokenLoginAction }}
              </button>
            </div>
          </template>
          <template v-else>
            <label>
              <span>{{ t.callsignField }}</span>
              <input v-model="registerForm.callsign" type="text" maxlength="6" />
            </label>
            <label>
              <span>{{ t.realName }}</span>
              <input v-model="registerForm.name" type="text" />
            </label>
            <label>
              <span>{{ t.phoneField }}</span>
              <input v-model="registerForm.phone" type="text" inputmode="numeric" />
            </label>
            <label>
              <span>{{ t.emailField }}</span>
              <input v-model="registerForm.mail" type="email" />
            </label>
            <label class="full-width">
              <span>{{ t.password }}</span>
              <input v-model="registerForm.password" type="password" autocomplete="new-password" />
            </label>
            <label class="full-width">
              <span>{{ t.addressField }}</span>
              <input v-model="registerForm.address" type="text" />
            </label>
            <label class="full-width upload-field">
              <span>{{ t.licenseUpload }}</span>
              <input type="file" accept="image/*" @change="onRegisterImageChange" />
              <small>{{ t.registerPhotoHint }}</small>
              <strong v-if="registerLicense">{{ registerLicense.name }} · {{ formatBytes(registerLicense.size) }}</strong>
            </label>
            <div class="auth-tip">
              {{ t.registerPendingHint }}
            </div>
            <div class="auth-actions">
              <button class="ghost-btn" :disabled="registerBusy" @click="backToLoginForm">
                {{ t.backToLogin }}
              </button>
              <button class="primary-btn" :disabled="registerBusy" @click="submitRegister">
                {{ registerBusy ? t.registering : t.registerAction }}
              </button>
            </div>
          </template>
        </div>

        <div v-if="showRegister ? registerError : loginError" class="auth-error">
          {{ showRegister ? registerError : loginError }}
        </div>
        <div v-if="registerSuccess" class="auth-success">{{ registerSuccess }}</div>

        <template v-if="!showRegister && platform.loggedIn && platform.user">
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

    <!-- 群组在线设备弹窗 -->
    <transition name="drawer-fade">
      <div v-if="groupDevicesPopup" class="drawer-backdrop" @click="groupDevicesPopup = null"></div>
    </transition>
    <transition name="drawer-fade">
      <div v-if="groupDevicesPopup" class="device-popup">
        <div class="drawer-head">
          <div>
            <h2>{{ groupDevicesPopup.group.id }} · {{ groupDevicesPopup.group.name }}</h2>
          </div>
          <button class="ghost-btn compact-ghost" @click="groupDevicesPopup = null">{{ t.close }}</button>
        </div>
        <div class="device-popup-list">
          <div v-if="groupDevicesLoading" class="ops-empty">…</div>
          <div v-else-if="groupDevicesPopup.devices.length === 0" class="ops-empty">
            {{ t.noOnlineDevices }}
          </div>
          <article v-for="device in groupDevicesPopup.devices" :key="device.id" class="roster-card">
            <div>
              <strong>{{ device.callsign }}-{{ device.ssid }}</strong>
              <p>{{ device.name || device.qth || t.onlineDevice }}</p>
            </div>
            <span v-if="device.isOnline" class="device-online-dot"></span>
          </article>
        </div>
      </div>
    </transition>

    <!-- QSO 呼叫弹窗 -->
    <transition name="drawer-fade">
      <div v-if="qsoDialogOpen" class="drawer-backdrop" @click="qsoDialogOpen = false"></div>
    </transition>
    <transition name="drawer-fade">
      <div v-if="qsoDialogOpen" class="device-popup">
        <div class="drawer-head">
          <div><h2>FMO QSO</h2></div>
          <button class="ghost-btn compact-ghost" @click="qsoDialogOpen = false">{{ t.close }}</button>
        </div>
        <div class="device-popup-list">
          <div v-if="fmo.qso.phase !== 'idle'" class="qso-status" :data-phase="fmo.qso.phase">
            <div class="qso-status-main">
              <strong>{{ qsoPhaseText }}</strong>
              <span>
                {{ fmo.qso.peer }}
                <template v-if="fmo.qso.peerUid">· uid {{ fmo.qso.peerUid }}</template>
              </span>
              <small v-if="fmo.qso.detail">{{ fmo.qso.detail }}</small>
            </div>
            <button class="ghost-btn compact" @click="cancelQso">
              {{
                fmo.qso.phase === "established"
                  ? language === "zh" ? "结束" : "End"
                  : language === "zh" ? "取消" : "Cancel"
              }}
            </button>
          </div>
          <div class="qso-form">
            <input
              v-model="qsoTargetCallsign"
              class="text-input"
              :placeholder="language === 'zh' ? '对方呼号' : 'Peer callsign'"
              @keydown.enter.prevent="startQsoCall"
            />
            <input v-model.number="qsoTargetUid" class="text-input qso-uid-input" placeholder="UID" />
            <button
              class="primary-btn"
              :disabled="fmo.busy || fmo.qso.phase !== 'idle' || !qsoTargetCallsign.trim()"
              @click="startQsoCall"
            >
              {{ language === "zh" ? "呼叫" : "Call" }}
            </button>
          </div>
          <div class="qso-pick-head">
            {{ language === "zh" ? "从在线用户选择（带 UID）" : "Pick from online users (with UID)" }}
          </div>
          <div class="qso-pick-list">
            <button
              v-for="c in fmo.state.clients.filter((x) => x.uid).slice(0, 30)"
              :key="c.callsign"
              class="qso-pick-row"
              @click="pickQsoTarget(c)"
            >
              <strong>{{ c.callsign }}</strong>
              <span>uid {{ c.uid }}</span>
              <small>{{ c.status_text || c.comment || "" }}</small>
            </button>
            <small v-if="!fmo.state.clients.some((x) => x.uid)" class="fmo-cert-msg">
              {{
                language === "zh"
                  ? "暂无带 UID 的在线用户，可手动输入呼号 + UID"
                  : "No online users with UID; enter callsign + UID manually"
              }}
            </small>
          </div>
        </div>
      </div>
    </transition>

    <!-- QSO 来电弹窗（未开自动接受时；必须做出选择，点空白不关闭） -->
    <transition name="drawer-fade">
      <div v-if="fmo.qso.phase === 'incoming'" class="drawer-backdrop"></div>
    </transition>
    <transition name="drawer-fade">
      <div v-if="fmo.qso.phase === 'incoming'" class="device-popup qso-incoming">
        <div class="drawer-head">
          <div>
            <h2>{{ language === "zh" ? "QSO 来电" : "Incoming QSO" }}</h2>
          </div>
        </div>
        <div class="device-popup-list">
          <div class="qso-incoming-peer">
            <strong>{{ fmo.qso.peer }}</strong>
            <span v-if="fmo.qso.peerUid">uid {{ fmo.qso.peerUid }}</span>
          </div>
          <div class="auth-actions">
            <button class="ghost-btn" @click="answerQso(false)">
              {{ language === "zh" ? "拒绝" : "Reject" }}
            </button>
            <button class="primary-btn" @click="answerQso(true)">
              {{ language === "zh" ? "接受" : "Accept" }}
            </button>
          </div>
        </div>
      </div>
    </transition>

    <!-- FMO 用户详情弹窗 -->
    <transition name="drawer-fade">
      <div v-if="fmoUserPopup" class="drawer-backdrop" @click="fmoUserPopup = null"></div>
    </transition>
    <transition name="drawer-fade">
      <div v-if="fmoUserPopup" class="device-popup">
        <div class="drawer-head">
          <div>
            <h2>{{ fmoUserPopup.callsign }}</h2>
          </div>
          <button class="ghost-btn compact-ghost" @click="fmoUserPopup = null">{{ t.close }}</button>
        </div>
        <div class="device-popup-list fmo-user-detail">
          <div v-for="row in fmoUserDetailRows" :key="row.label" class="fmo-user-detail-row">
            <span>{{ row.label }}</span>
            <strong>{{ row.value }}</strong>
          </div>
          <div v-if="fmoUserPopup.recent?.length" class="fmo-user-detail-row fmo-user-detail-recent">
            <span>{{ language === "zh" ? "最近消息" : "Recent" }}</span>
            <div>
              <strong v-for="(m, i) in fmoUserPopup.recent" :key="i">
                {{ fmtClientDateTime(m.ts) }} {{ m.text }}
              </strong>
            </div>
          </div>
        </div>
      </div>
    </transition>

    <!-- FMO 服务器详情弹窗 -->
    <transition name="drawer-fade">
      <div v-if="fmoServerPopup" class="drawer-backdrop" @click="fmoServerPopup = null"></div>
    </transition>
    <transition name="drawer-fade">
      <div v-if="fmoServerPopup" class="device-popup">
        <div class="drawer-head">
          <div>
            <h2>{{ fmoServerPopup.name || fmoServerPopup.callsign || fmoServerPopup.host }}</h2>
          </div>
          <button class="ghost-btn compact-ghost" @click="fmoServerPopup = null">{{ t.close }}</button>
        </div>
        <div class="device-popup-list fmo-user-detail">
          <div v-for="row in fmoServerDetailRows" :key="row.label" class="fmo-user-detail-row">
            <span>{{ row.label }}</span>
            <strong>{{ row.value }}</strong>
          </div>
        </div>
      </div>
    </transition>
  </main>
</template>
