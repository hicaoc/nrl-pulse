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
  server: string;
  port: number;
  serverName: string;
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
}

export interface SessionSnapshot {
  roomName: string;
  callsign: string;
  ssid: number;
  activeSpeaker: string;
  activeSpeakerSsid: number;
  connection: ConnectionPhase;
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
  isMonitoring: boolean;
  queuedFrames: number;
  lastTextMessage: string;
  devices: DeviceSettings;
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
