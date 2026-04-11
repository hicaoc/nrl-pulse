import { invoke } from "@tauri-apps/api/core";
import type {
  GroupSnapshot,
  LoginBootstrap,
  PlatformDevice,
  PlatformServer,
} from "@/types";

export async function fetchPlatformServers(): Promise<PlatformServer[]> {
  return invoke<PlatformServer[]>("fetch_platform_servers");
}

export async function platformLogin(
  server: PlatformServer,
  username: string,
  password: string,
): Promise<LoginBootstrap> {
  return invoke<LoginBootstrap>("platform_login", { server, username, password });
}

export async function platformRestoreSession(
  apiBase: string,
  token: string,
  server: PlatformServer,
  currentGroupId: number,
): Promise<LoginBootstrap> {
  return invoke<LoginBootstrap>("platform_restore_session", {
    apiBase,
    token,
    server,
    currentGroupId,
  });
}

export async function platformFetchGroups(
  apiBase: string,
  token: string,
  currentGroupId: number,
): Promise<GroupSnapshot> {
  return invoke<GroupSnapshot>("platform_fetch_groups", { apiBase, token, currentGroupId });
}

export async function platformFetchGroupDevices(
  apiBase: string,
  token: string,
  groupId: number,
): Promise<PlatformDevice[]> {
  return invoke<PlatformDevice[]>("platform_fetch_group_devices", {
    apiBase,
    token,
    groupId,
  });
}

export async function platformSwitchGroup(
  apiBase: string,
  token: string,
  callsign: string,
  ssid: number,
  groupId: number,
): Promise<GroupSnapshot> {
  return invoke<GroupSnapshot>("platform_switch_group", {
    apiBase,
    token,
    callsign,
    ssid,
    groupId,
  });
}
