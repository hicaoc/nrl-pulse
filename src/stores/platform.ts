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
  const runtime = useRuntimeStore();
  const servers = ref<PlatformServer[]>([]);
  const selectedServerHost = ref("");
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
    selectedServerHost.value = runtime.config.server || servers.value[0]?.host || "";
    username.value = runtime.config.loginUsername || "";
    if (runtime.config.apiBase && runtime.config.authToken && selectedServerHost.value) {
      const server =
        servers.value.find((item) => item.host === selectedServerHost.value) ??
        ({
          name: runtime.config.serverName || selectedServerHost.value,
          host: selectedServerHost.value,
          port: String(runtime.config.port || 10024),
          online: 0,
          total: 0,
        } as PlatformServer);
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
  }

  function applyBootstrap(data: LoginBootstrap) {
    apiBase.value = data.apiBase;
    token.value = data.token;
    user.value = data.user;
    groups.value = data.groups;
    devices.value = data.devices;
    currentGroupId.value = data.currentGroupId;
    selectedServerHost.value = data.server.host;
  }

  async function login() {
    const server = servers.value.find((item) => item.host === selectedServerHost.value);
    if (!server) {
      throw new Error("请选择登录服务器");
    }
    busy.value = true;
    try {
      const data = await platformLogin(server, username.value.trim(), password.value);
      applyBootstrap(data);
      const currentGroupName =
        data.groups.find((group) => group.id === data.currentGroupId)?.name ?? runtime.config.roomName;
      await runtime.saveConfig({
        ...runtime.config,
        server: data.server.host,
        port: Number(data.server.port),
        serverName: data.server.name,
        apiBase: data.apiBase,
        authToken: data.token,
        loginUsername: username.value.trim(),
        callsign: data.user.callsign || runtime.config.callsign,
        roomName: currentGroupName,
        currentGroupId: data.currentGroupId,
      });
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
      await runtime.saveConfig({
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
    login,
    refreshGroups,
    switchGroup,
    logout,
  };
});
