import { computed, ref } from "vue";
import { defineStore } from "pinia";
import {
  fetchPlatformServers,
  platformFetchGroups,
  platformLogin,
  platformRestoreSession,
  platformSwitchGroup,
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

export const usePlatformStore = defineStore("platform", () => {
  const DEFAULT_CUSTOM_SERVER_PORT = "60050";
  const runtime = useRuntimeStore();
  const servers = ref<PlatformServer[]>([]);
  const selectedServerHost = ref("");
  const useCustomServer = ref(false);
  const customServerHost = ref("");
  const username = ref("");
  const password = ref("");
  const apiBase = ref("");
  const token = ref("");
  const user = ref<PlatformUser | null>(null);
  const groups = ref<PlatformGroup[]>([]);
  const devices = ref<PlatformDevice[]>([]);
  const currentGroupId = ref(0);
  const busy = ref(false);
  const loaded = ref(false);
  const loggedIn = computed(() => !!token.value && !!user.value);
  const onlineDevices = computed(() => devices.value.filter((device) => device.isOnline));
  const currentGroup = computed(
    () => groups.value.find((group) => group.id === currentGroupId.value) ?? null,
  );

  async function bootstrap() {
    if (loaded.value) {
      return;
    }
    await refreshServers();
    hydrateServerSelection(runtime.config.server || "");
    username.value = runtime.config.loginUsername || "";
    const server = resolveSelectedServer();
    if (runtime.config.apiBase && runtime.config.authToken && server) {
      try {
        const data = await platformRestoreSession(
          runtime.config.apiBase,
          runtime.config.authToken,
          server,
          runtime.config.currentGroupId,
        );
        applyBootstrap(data);
      } catch {
        logout();
      }
    }
    loaded.value = true;
  }

  async function refreshServers() {
    servers.value = await fetchPlatformServers();
    if (!useCustomServer.value && selectedServerHost.value) {
      const matched = servers.value.find((item) => item.host === selectedServerHost.value);
      if (!matched) {
        selectedServerHost.value = servers.value[0]?.host || "";
      }
    } else if (!useCustomServer.value && !selectedServerHost.value) {
      selectedServerHost.value = servers.value[0]?.host || "";
    }
  }

  function hydrateServerSelection(serverHost: string) {
    const trimmed = serverHost.trim();
    if (!trimmed) {
      useCustomServer.value = false;
      customServerHost.value = "";
      selectedServerHost.value = servers.value[0]?.host || "";
      return;
    }
    const matched = servers.value.find((item) => item.host === trimmed);
    if (matched) {
      useCustomServer.value = false;
      customServerHost.value = "";
      selectedServerHost.value = matched.host;
      return;
    }
    useCustomServer.value = true;
    customServerHost.value = trimmed;
    selectedServerHost.value = "";
  }

  function resolveSelectedServer(): PlatformServer | null {
    if (useCustomServer.value) {
      const host = customServerHost.value.trim().replace(/\/+$/, "");
      if (!host) {
        return null;
      }
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
    return servers.value.find((item) => item.host === selectedServerHost.value) ?? null;
  }

  function applyBootstrap(data: LoginBootstrap) {
    apiBase.value = data.apiBase;
    token.value = data.token;
    user.value = data.user;
    groups.value = data.groups;
    devices.value = data.devices;
    currentGroupId.value = data.currentGroupId;
    hydrateServerSelection(data.server.host);
  }

  function shouldReconnectAfterLogin(data: LoginBootstrap) {
    if (runtime.snapshot.connection !== "connected") {
      return false;
    }
    return (
      runtime.config.server !== data.server.host ||
      runtime.config.port !== Number(data.server.port) ||
      runtime.config.callsign !== (data.user.callsign || runtime.config.callsign)
    );
  }

  async function login() {
    const server = resolveSelectedServer();
    if (!server) {
      throw new Error(useCustomServer.value ? "请输入登录服务器" : "请选择登录服务器");
    }
    busy.value = true;
    try {
      const data = await platformLogin(server, username.value.trim(), password.value);
      const reconnectNeeded = shouldReconnectAfterLogin(data);
      applyBootstrap(data);
      const currentGroupName =
        data.groups.find((group) => group.id === data.currentGroupId)?.name ?? runtime.config.roomName;
      const nextConfig = {
        ...runtime.config,
        server: data.server.host,
        port: Number(data.server.port),
        serverName: data.server.name || server.name,
        apiBase: data.apiBase,
        authToken: data.token,
        loginUsername: username.value.trim(),
        callsign: data.user.callsign || runtime.config.callsign,
        roomName: currentGroupName,
        currentGroupId: data.currentGroupId,
      };
      if (reconnectNeeded) {
        await runtime.reconnectWithConfig(nextConfig);
      } else {
        await runtime.saveConfig(nextConfig);
      }
      password.value = "";
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
      // 后台持久化配置，不阻塞 platform.busy：
      // 群组切换的关键操作是 HTTP 平台 API，config save 是次要的磁盘持久化，
      // 不应将两个 busy 串联起来导致按钮长时间禁用
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

  async function logout() {
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
      currentGroupId: 0,
    });
  }

  return {
    servers,
    selectedServerHost,
    useCustomServer,
    customServerHost,
    username,
    password,
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
    loggedIn,
    bootstrap,
    refreshServers,
    resolveSelectedServer,
    login,
    refreshGroups,
    switchGroup,
    logout,
  };
});
