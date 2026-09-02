import { computed, ref } from "vue";
import { defineStore } from "pinia";
import {
  fetchPlatformServers,
  platformFetchGroups,
  platformLogin,
  platformLoginWithToken,
  platformRestoreSession,
  platformSwitchGroup,
  platformFetchDeviceGroup,
} from "@/lib/platform";
import { useRuntimeStore } from "@/stores/runtime";
import type {
  GroupSnapshot,
  LoginBootstrap,
  PlatformDevice,
  PlatformGroup,
  PlatformServer,
  PlatformUser,
} from "@/types";

const DEFAULT_CUSTOM_SERVER_PORT = "60050";

function normalizeHost(value: string): string {
  const trimmed = value.trim().replace(/\/+$/, "");
  if (!trimmed) return "";
  try {
    if (trimmed.startsWith("http://") || trimmed.startsWith("https://")) {
      return new URL(trimmed).hostname;
    }
  } catch {
    // 输入中的非法 URL 按原文本处理，后续 base_candidates 仍可尝试。
  }
  return trimmed;
}

function hostFromApiBase(apiBase: string): string {
  try {
    return new URL(apiBase).hostname;
  } catch {
    return "";
  }
}

export const usePlatformStore = defineStore("platform", () => {
  const runtime = useRuntimeStore();
  const servers = ref<PlatformServer[]>([]);

  // NRL 语音服务器：仅依赖呼号/SSID + UDP 心跳，不要求登录。
  const voiceServerHost = ref("");
  const useCustomVoiceServer = ref(false);
  const customVoiceServerHost = ref("");

  // NRL 管理登录服务器：Token / 账号密码登录，用于群组和设备管理。
  const authServerHost = ref("");
  const useCustomAuthServer = ref(false);
  const customAuthServerHost = ref("");

  const username = ref("");
  const password = ref("");
  const hamidToken = ref("");
  const apiBase = ref("");
  const token = ref("");
  const user = ref<PlatformUser | null>(null);
  const groups = ref<PlatformGroup[]>([]);
  const devices = ref<PlatformDevice[]>([]);
  const currentGroupId = ref(0);
  const busy = ref(false);
  const loaded = ref(false);
  // 服务器列表拉取失败的原因（TLS/代理/防火墙等），供界面直接展示，
  // 否则列表为空时用户无法区分「没有服务器」和「网络故障」
  const serversError = ref("");

  const loggedIn = computed(() => !!token.value && !!user.value);
  const onlineDevices = computed(() => devices.value.filter((device) => device.isOnline));
  const currentGroup = computed(
    () => groups.value.find((group) => group.id === currentGroupId.value) ?? null,
  );
  const authServerLabel = computed(() => {
    const host = authServerHost.value || hostFromApiBase(apiBase.value);
    return host || "-";
  });
  const loggedInOnVoiceServer = computed(
    () => loggedIn.value && normalizeHost(authServerHost.value) === normalizeHost(runtime.config.server),
  );

  function findServer(host: string): PlatformServer | undefined {
    const normalized = normalizeHost(host);
    return servers.value.find((item) => normalizeHost(item.host) === normalized);
  }

  async function bootstrap() {
    if (loaded.value) {
      return;
    }
    await refreshServers();

    hydrateVoiceServer(runtime.config.server || "");
    hydrateAuthServer(runtime.config.authServer || hostFromApiBase(runtime.config.apiBase || ""));
    username.value = runtime.config.loginUsername || "";

    if (runtime.config.apiBase && runtime.config.authToken) {
      const server = resolveAuthServer();
      if (server) {
        try {
          const data = await platformRestoreSession(
            runtime.config.apiBase,
            runtime.config.authToken,
            server,
            runtime.config.currentGroupId,
          );
          applyAuthBootstrap(data);
          await syncCurrentDeviceGroup();
        } catch {
          await logout({ keepCallsign: true });
        }
      }
    }
    loaded.value = true;
  }

  async function refreshServers() {
    try {
      servers.value = await fetchPlatformServers();
      serversError.value = "";
    } catch (error) {
      // 失败时保留旧列表，只记录原因；不再向上抛，避免阻塞登录态恢复
      serversError.value = error instanceof Error ? error.message : String(error);
    }
    hydrateVoiceServer(runtime.config.server || voiceServerHost.value);
    hydrateAuthServer(authServerHost.value || hostFromApiBase(apiBase.value || ""));
  }

  function hydrateVoiceServer(serverHost: string) {
    const host = normalizeHost(serverHost || runtime.config.server);
    if (!host) {
      useCustomVoiceServer.value = false;
      customVoiceServerHost.value = "";
      voiceServerHost.value = servers.value[0]?.host || "";
      return;
    }
    const matched = findServer(host);
    if (matched) {
      useCustomVoiceServer.value = false;
      customVoiceServerHost.value = "";
      voiceServerHost.value = normalizeHost(matched.host);
      return;
    }
    useCustomVoiceServer.value = true;
    customVoiceServerHost.value = host;
    voiceServerHost.value = "";
  }

  function hydrateAuthServer(serverHost: string) {
    const host = normalizeHost(serverHost);
    if (!host) {
      useCustomAuthServer.value = false;
      customAuthServerHost.value = "";
      authServerHost.value = servers.value[0]?.host || "";
      return;
    }
    const matched = findServer(host);
    if (matched) {
      useCustomAuthServer.value = false;
      customAuthServerHost.value = "";
      authServerHost.value = normalizeHost(matched.host);
      return;
    }
    useCustomAuthServer.value = true;
    customAuthServerHost.value = host;
    authServerHost.value = "";
  }

  function resolveVoiceServer(): PlatformServer | null {
    if (useCustomVoiceServer.value) {
      const host = normalizeHost(customVoiceServerHost.value);
      if (!host) return null;
      const savedPort =
        runtime.config.server === host && runtime.config.port
          ? String(runtime.config.port)
          : DEFAULT_CUSTOM_SERVER_PORT;
      return {
        name: runtime.config.serverName || host,
        host,
        port: savedPort,
        online: 0,
        total: 0,
      };
    }
    return servers.value.find(
      (item) => normalizeHost(item.host) === normalizeHost(voiceServerHost.value),
    ) ?? null;
  }

  function resolveAuthServer(): PlatformServer | null {
    if (useCustomAuthServer.value) {
      const host = normalizeHost(customAuthServerHost.value);
      if (!host) return null;
      return {
        name: host,
        host,
        port: DEFAULT_CUSTOM_SERVER_PORT,
        online: 0,
        total: 0,
      };
    }
    return servers.value.find(
      (item) => normalizeHost(item.host) === normalizeHost(authServerHost.value),
    ) ?? null;
  }

  function applyAuthBootstrap(data: LoginBootstrap) {
    apiBase.value = data.apiBase;
    token.value = data.token;
    user.value = data.user;
    groups.value = data.groups;
    devices.value = data.devices;
    currentGroupId.value = data.currentGroupId;
    authServerHost.value = normalizeHost(data.server.host);
    hydrateAuthServer(data.server.host);
  }

  function shouldReconnectAfterLogin(data: LoginBootstrap) {
    return (
      runtime.snapshot.connection === "connected" &&
      runtime.config.callsign !== (data.user.callsign || runtime.config.callsign)
    );
  }

  async function login() {
    const server = resolveAuthServer();
    if (!server) {
      throw new Error(useCustomAuthServer.value ? "请输入管理服务器" : "请选择管理服务器");
    }
    busy.value = true;
    try {
      const data = await platformLogin(server, username.value.trim(), password.value);
      await persistLogin(data, username.value.trim());
      await syncCurrentDeviceGroup();
      password.value = "";
    } finally {
      busy.value = false;
    }
  }

  async function persistLogin(data: LoginBootstrap, loginUsername: string) {
    const reconnectNeeded = shouldReconnectAfterLogin(data);
    applyAuthBootstrap(data);

    const currentGroupName =
      data.groups.find((group) => group.id === data.currentGroupId)?.name ?? runtime.config.roomName;
    const nextConfig = {
      ...runtime.config,
      authServer: normalizeHost(data.server.host),
      authServerName: data.server.name || normalizeHost(data.server.host),
      apiBase: data.apiBase,
      authToken: data.token,
      loginUsername,
      callsign: data.user.callsign || runtime.config.callsign,
      roomName: currentGroupName,
      currentGroupId: data.currentGroupId,
    };
    // 语音服务器仍是出厂默认（127.0.0.1/Local，用户从未选择）时，登录成功后把
    // 语音服务器对齐到登录服务器并发起连接，否则顶部状态/呼号栏一直停留在
    // Local + 公共大厅，看起来像登录没生效。
    const voiceUntouched = ["", "127.0.0.1", "localhost", "::1"].includes(
      normalizeHost(runtime.config.server).toLowerCase(),
    );
    const authHost = normalizeHost(data.server.host);
    const adoptVoiceServer = voiceUntouched && !!authHost;
    if (adoptVoiceServer) {
      nextConfig.server = authHost;
      nextConfig.serverName = data.server.name || authHost;
      nextConfig.port = Number(data.server.port || DEFAULT_CUSTOM_SERVER_PORT) || Number(DEFAULT_CUSTOM_SERVER_PORT);
      voiceServerHost.value = authHost;
      useCustomVoiceServer.value = false;
      customVoiceServerHost.value = "";
    }
    if (reconnectNeeded) {
      await runtime.reconnectWithConfig(nextConfig);
    } else {
      await runtime.saveConfig(nextConfig);
      if (
        adoptVoiceServer &&
        runtime.snapshot.connection !== "connected" &&
        runtime.snapshot.connection !== "connecting"
      ) {
        await runtime.connect();
      }
    }
  }

  async function syncCurrentDeviceGroup() {
    if (!runtime.config.callsign) return;
    try {
      const groupId = await platformFetchDeviceGroup(
        runtime.config.server,
        runtime.config.callsign,
        runtime.config.ssid,
      );
      currentGroupId.value = groupId;
      const groupName = groups.value.find((group) => group.id === groupId)?.name;
      await runtime.saveConfig({
        ...runtime.config,
        currentGroupId: groupId,
        roomName: groupName || runtime.config.roomName,
      });
    } catch {
      // 查询失败时保留当前组，不影响登录/语音连接。
    }
  }

  async function loginWithToken(token: string) {
    const server = resolveAuthServer();
    if (!server) {
      throw new Error(useCustomAuthServer.value ? "请输入管理服务器" : "请选择管理服务器");
    }
    const trimmedToken = token.trim();
    if (!trimmedToken.startsWith("hamid_pat_")) {
      throw new Error("HAM ID Token 格式无效");
    }

    busy.value = true;
    try {
      const data = await platformLoginWithToken(server, trimmedToken);
      await persistLogin(data, data.user.callsign || trimmedToken);
      await syncCurrentDeviceGroup();
      hamidToken.value = "";
    } finally {
      busy.value = false;
    }
  }

  async function refreshGroups() {
    if (!loggedIn.value) {
      return;
    }
    busy.value = true;
    try {
      const data = await platformFetchGroups(apiBase.value, token.value, currentGroupId.value);
      applyGroupSnapshot(data);
    } finally {
      busy.value = false;
    }
  }

  function applyGroupSnapshot(data: GroupSnapshot) {
    groups.value = data.groups;
    devices.value = data.devices;
    currentGroupId.value = data.currentGroupId;
  }

  async function switchGroup(groupId: number) {
    if (!loggedIn.value || !user.value) {
      throw new Error("请先登录");
    }
    busy.value = true;
    try {
      const data = await platformSwitchGroup(
        apiBase.value,
        token.value,
        user.value.callsign,
        runtime.config.ssid,
        groupId,
      );
      applyGroupSnapshot(data);
      const groupName =
        data.groups.find((group) => group.id === data.currentGroupId)?.name ?? runtime.config.roomName;
      void runtime.saveConfig({
        ...runtime.config,
        authToken: token.value,
        loginUsername: username.value.trim(),
        callsign: user.value.callsign,
        roomName: groupName,
        currentGroupId: data.currentGroupId,
      });
    } finally {
      busy.value = false;
    }
  }

  async function selectVoiceServer(server: PlatformServer) {
    const host = normalizeHost(server.host);
    if (!host) return;
    const port = Number(server.port || DEFAULT_CUSTOM_SERVER_PORT);
    const nextConfig = {
      ...runtime.config,
      server: host,
      port,
      serverName: server.name || host,
    };
    hydrateVoiceServer(host);
    if (runtime.snapshot.connection === "connected") {
      await runtime.reconnectWithConfig(nextConfig);
      await syncCurrentDeviceGroup();
    } else {
      await runtime.saveConfig(nextConfig);
      await syncCurrentDeviceGroup();
    }
  }

  async function selectCustomVoiceServer(hostText: string, portText: string) {
    const host = normalizeHost(hostText);
    if (!host) {
      throw new Error("请输入 NRL 服务器地址");
    }
    const port = Number(portText || DEFAULT_CUSTOM_SERVER_PORT);
    if (!Number.isInteger(port) || port <= 0 || port > 65535) {
      throw new Error("NRL 服务器端口无效");
    }
    await selectVoiceServer({
      id: null,
      name: host,
      host,
      port: String(port),
      online: 0,
      total: 0,
    });
  }

  async function logout(options?: { keepCallsign?: boolean }) {
    token.value = "";
    apiBase.value = "";
    user.value = null;
    groups.value = [];
    devices.value = [];
    currentGroupId.value = 0;
    password.value = "";
    await runtime.saveConfig({
      ...runtime.config,
      authToken: "",
      apiBase: "",
      loginUsername: username.value.trim(),
      currentGroupId: options?.keepCallsign ? runtime.config.currentGroupId : 0,
    });
  }

  return {
    servers,
    voiceServerHost,
    useCustomVoiceServer,
    customVoiceServerHost,
    authServerHost,
    useCustomAuthServer,
    customAuthServerHost,
    authServerLabel,
    loggedInOnVoiceServer,
    username,
    password,
    hamidToken,
    apiBase,
    token,
    user,
    groups,
    devices,
    currentGroupId,
    currentGroup,
    onlineDevices,
    busy,
    loaded,
    serversError,
    loggedIn,
    bootstrap,
    refreshServers,
    hydrateVoiceServer,
    resolveVoiceServer,
    selectVoiceServer,
    selectCustomVoiceServer,
    hydrateAuthServer,
    resolveAuthServer,
    login,
    loginWithToken,
    refreshGroups,
    syncCurrentDeviceGroup,
    switchGroup,
    logout,
  };
});
