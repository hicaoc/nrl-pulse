export type ConnectionPhase =
  | "disconnected"
  | "connecting"
  | "connected"
  | "recovering";

export interface DeviceSettings {
  inputDevice: string;
  outputDevice: string;
  sampleRate: number;
  jitterBufferMs: number;
  agcEnabled: boolean;
  noiseSuppression: boolean;
  aecEnabled: boolean;
}

export interface RuntimeConfig {
  protocol: "nrl" | "fmo";
  voiceCodec: "alaw" | "opus";
  fmoVoiceMode: "adpcm" | "opus";
  fmoCallsign: string;
  server: string;
  port: number;
  serverName: string;
  authServer: string;
  authServerName: string;
  apiBase: string;
  authToken: string;
  loginUsername: string;
  callsign: string;
  ssid: number;
  roomName: string;
  currentGroupId: number;
  volume: number;
  pttKey: string;
  voiceSavePath: string;
  serialTunnel: SerialTunnelConfig;
}

export interface SerialTunnelConfig {
  mode: "physical";
  autoStart: boolean;
  portName: string;
  baudRate: number;
  dataBits: number;
  parity: "none" | "odd" | "even";
  stopBits: "one" | "two";
  flowControl: "none" | "software" | "hardware";
}

export interface SerialTunnelSnapshot {
  running: boolean;
  supported: boolean;
  mode: "physical";
  portName: string;
  status: string;
  rxBytes: number;
  txBytes: number;
  lastError: string;
}

export interface RealtimeAudioState {
  activeSpeaker: string;
  activeSpeakerSsid: number;
  rxLevel: number;
  txLevel: number;
  rxSpectrum: number[];
  txSpectrum: number[];
  queuedFrames: number;
  uplinkKbps: number;
  downlinkKbps: number;
  isTransmitting: boolean;
  txProtocol: "nrl" | "fmo";
  rxCodec: string;
  rxSeq: number;
}

export interface SessionSnapshot {
  roomName: string;
  callsign: string;
  ssid: number;
  activeSpeaker: string;
  activeSpeakerSsid: number;
  connection: ConnectionPhase;
  nrlConnected: boolean;
  packetLoss: number;
  latencyMs: number;
  jitterMs: number;
  uplinkKbps: number;
  downlinkKbps: number;
  rxLevel: number;
  txLevel: number;
  rxSpectrum: number[];
  txSpectrum: number[];
  isTransmitting: boolean;
  // 语音互转（桥接）模式：0=关闭, 1=FMO→NRL, 2=NRL→FMO, 3=双向
  bridgeMode: number;
  // 桥接发射状态：对应方向正在向外转发语音时点亮对应协议的 PTT 按钮
  bridgeTxNrl: boolean;
  bridgeTxFmo: boolean;
  txProtocol: "nrl" | "fmo";
  isMonitoring: boolean;
  queuedFrames: number;
  lastTextMessage: string;
  devices: DeviceSettings;
  nrlLastRxMs: number;
  // 由 runtime://audio-state 高频事件合并进来（后端整包 snapshot 不含这两项）
  rxCodec?: string;
  rxSeq?: number;
}

export interface PresenceItem {
  id: string;
  callsign: string;
  ssid: number;
  role: string;
  state: "online" | "speaking" | "idle";
  signal: number;
  lastSeen: string;
}

export interface TimelineEvent {
  id: string;
  time: string;
  title: string;
  detail: string;
  tone: "info" | "warn" | "accent";
}

export interface ChatMessageEvent {
  id: string;
  side: "self" | "remote";
  text: string;
  meta: string;
  time: string;
  type?: "text" | "voice";
  waveform?: number[];
  filePath?: string;
  duration?: number;
}

export interface PlatformServer {
  id?: number;
  name: string;
  host: string;
  port: string;
  online: number;
  total: number;
}

export interface PlatformUser {
  id: number;
  name: string;
  callsign: string;
  nickname?: string;
  avatar?: string;
  roles: string[];
}

export interface PlatformGroup {
  id: number;
  name: string;
  groupType: number;
  onlineDevNumber: number;
  totalDevNumber: number;
}

export interface PlatformDevice {
  id: number;
  name: string;
  callsign: string;
  ssid: number;
  groupId: number;
  devModel?: number;
  qth?: string;
  note?: string;
  isOnline: boolean;
}

export interface LoginBootstrap {
  apiBase: string;
  token: string;
  user: PlatformUser;
  groups: PlatformGroup[];
  currentGroupId: number;
  devices: PlatformDevice[];
  server: PlatformServer;
}

export interface GroupSnapshot {
  groups: PlatformGroup[];
  currentGroupId: number;
  devices: PlatformDevice[];
}

export interface PlatformRegisterPayload {
  callsign: string;
  name: string;
  phone: string;
  password: string;
  address: string;
  mail: string;
}

export interface PlatformRegisterResult {
  code: number;
  message?: string;
}

// ---------------------------------------------------------------- FMO

export interface FmoIdentity {
  callsign: string;
  uid: number;
}

export interface FmoCertEntry {
  name: string;
  fingerprint: string;
  source: string;
  info: string;
  valid: boolean;
  imported_at: number;
  file: string;
}

export interface FmoServer {
  key: string;
  host: string;
  port?: number;
  callsign: string;
  name?: string;
  source?: string;
  first_seen?: number;
  last_seen?: number;
  online?: number;
  total?: number;
  cover_km?: number;
  freq?: number;
  height?: number;
  uid?: number;
  subtype?: string;
  version?: string;
  s_code?: number;
  country?: string;
  status_text?: string;
  rig?: string;
  ant?: string;
  cert?: {
    callsign: string;
    uid: number;
    pubkey_hex: string;
    [key: string]: unknown;
  };
  lat?: string;
  lon?: string;
  raw?: string;
}

export interface FmoFavorite {
  key: string;
  host: string;
  port?: number;
  callsign?: string;
  name?: string;
  uid?: number;
  online?: number;
  total?: number;
  favorited_at?: number;
}

export interface FmoServerTraffic {
  count: number;
  rawFrames: number;
  tele: number;
  serverInfo: number;
  profile: number;
  lastTopic: string;
  lastMsg: string;
  lastTs: number;
}

export interface FmoClient {
  callsign: string;
  uid?: number;
  kind?: string;
  subtype?: string;
  status_text?: string;
  comment?: string;
  freq?: number;
  rig?: string;
  height?: number;
  ant?: string;
  recent?: { ts: number; text: string }[];
  version?: string;
  lat?: string;
  lon?: string;
  first_seen?: number;
  last_seen?: number;
}

export interface FmoStateSnapshot {
  identity: FmoIdentity;
  certCallsign: string;
  passcode: string;
  certs: FmoCertEntry[];
  favorites: FmoFavorite[];
  servers: FmoServer[];
  clients: FmoClient[];
  mqttState: string;
  mqttDetail: string;
  mqttClientId: string;
  aprsState: string;
  aprsDetail: string;
  selectedServer: Partial<FmoServer> | null;
  rxPlay: boolean;
  mqttNoLocal: boolean;
}

export interface FmoStatsSnapshot {
  callsign: string;
  uid: number;
  mqttState: string;
  mqttDetail: string;
  mqttClientId: string;
  mqttRole: string;
  aprsState: string;
  aprsDetail: string;
  serverHost: string;
  serverPort: number;
  serverName: string;
  activeSpeaker: string;
  presenceOnline: number;
  presencePeak: number;
  broadcastOnline: number;
  broadcastPeak: number;
  beaconEnabled: boolean;
  beaconLastSent: number;
  rxFrames: number;
  txFrames: number;
  rxText: number;
  txPackets: number;
  serverInfo: number;
  rxLevel: number;
  rxSpectrum: number[];
  txLevel: number;
  txSpectrum: number[];
  jitterMs: number;
  latencyMs: number;
  packetLoss: number;
  queuedFrames: number;
  downlinkKbps: number;
  uplinkKbps: number;
  rxCodec: string;
  /** 说话人位置（后端解算，对齐原厂）：网格 / 距离km / 方位角 / 罗盘方位 / 来源 */
  speakerGrid?: string;
  speakerDistanceKm?: number;
  speakerBearingDeg?: number;
  speakerCompass?: string;
  /** beacon=APRS 信标经纬度（精确）；grid=成员 JSON 网格（±10km，显示加 ≈） */
  speakerPosSource?: "beacon" | "grid";
}

export interface FmoQsoState {
  phase: "idle" | "querying" | "calling" | "ringing" | "incoming" | "established";
  peer: string;
  peerUid: number;
  outgoing: boolean;
  detail?: string;
  autoAccept?: boolean;
}

export interface FmoQsoRecord {
  ts: number;
  dir: "in" | "out";
  peer: string;
  peer_uid: number;
  result: string;
  /** 祝福/备注（收到的完整通联记录的 toComment；本地信令记录无此字段） */
  comment?: string;
  /** 对方梅登黑德网格（收到的完整通联记录） */
  grid?: string;
  /** 中继/服务器名（收到的完整通联记录） */
  relay?: string;
}

export interface FmoBroadcastConfig {
  mode_min: number;
  name: string;
  host: string;
  port: number;
  cover_km: number;
  online: number;
  peak: number;
  country: string;
  lat: number;
  lon: number;
  /** 广播呼号的 APRS SSID（0-15，0=不带；包头和 TBS 签同一个值） */
  ssid: number;
}

/** 个人信标（BEACON）配置（beacon.json，后端 serde(default) 全字段兜底） */
export interface FmoBeaconConfig {
  /** 周期信标开关（固定 10 分钟周期 + 60s 限速） */
  enabled: boolean;
  /** 信标呼号的 APRS SSID（0-15，0=不带） */
  ssid: number;
  /** 电台名称（≤16 字符，线上 UTF-8 / TBS 内同字节） */
  rig: string;
  /** 直频频率 MHz（>0 才发送；合法范围 20-500） */
  freq_mhz: number;
  /** 天线型号（≤16 字符） */
  ant: string;
  /** 天线高度 m（0=报文中省略 HEIGHT 段） */
  height_m: number;
  /** APRS 个性化消息（≤64 字符，BEACON 成功后以 APFMO2 跟发） */
  aprs_msg: string;
  /** 登录公告（≤128 字符，STATION 广播成功后以 APFMO1 跟发） */
  notice: string;
  /** QSO 祝福语（≤128 字符）：QSO 建立时随完整通联记录 JSON 发到对方 FMO/QSO/UID/<uid>（toComment 字段） */
  qso_msg: string;
}

export interface FmoEvent {
  type:
    | "log"
    | "mqtt_state"
    | "aprs_state"
    | "server_list"
    | "cert_state"
    | "favorites"
    | "server_traffic"
    | "qso_state"
    | "qso_log_changed"
    | "broadcast_state"
    | "beacon_state";
  [key: string]: unknown;
}
