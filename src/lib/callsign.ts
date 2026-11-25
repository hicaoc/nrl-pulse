/**
 * 呼号校验（与后端 config::normalize_callsign 规则一致）：
 * 基础呼号 4-6 位、仅字母数字（小写自动转大写）、至少一位数字；
 * 可带 `-数字` SSID 后缀（0-15）。空值表示未设置，放行。
 */

export function normalizeCallsign(input: string): string {
  return input.trim().toUpperCase();
}

/** 返回 null 表示合法，否则为可直接展示的错误原因。 */
export function callsignError(input: string): string | null {
  const cs = normalizeCallsign(input);
  if (!cs) return null;
  const dash = cs.indexOf("-");
  const base = dash >= 0 ? cs.slice(0, dash) : cs;
  const ssid = dash >= 0 ? cs.slice(dash + 1) : null;
  if (
    base.length < 4 ||
    base.length > 6 ||
    !/^[A-Z0-9]+$/.test(base) ||
    !/\d/.test(base)
  ) {
    return `呼号 ${input} 不合法：基础呼号需为 4-6 位字母或数字，且至少包含一位数字（如 BG9JYT）`;
  }
  if (ssid !== null) {
    const n = Number(ssid);
    if (!/^\d+$/.test(ssid) || !Number.isInteger(n) || n > 15) {
      return `呼号 ${input} 的 SSID 后缀不合法：应为 -0 到 -15 的数字`;
    }
  }
  return null;
}
