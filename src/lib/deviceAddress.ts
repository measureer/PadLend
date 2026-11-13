/** 无线调试地址补全与校验：与 tablet/adb.py 的 normalize_address 保持同一语义。 */

const DEFAULT_PORT = 5555;

/** 补全默认端口；IPv6 必须带方括号才能和端口共存。无法判断时返回空串。 */
export function normalizeDeviceAddress(raw: string): string {
  const value = raw.trim();
  if (!value) return "";
  if (value.startsWith("[")) {
    const close = value.indexOf("]");
    if (close <= 1) return "";
    const host = value.slice(1, close);
    const rest = value.slice(close + 1);
    if (!rest) return "[" + host + "]:" + DEFAULT_PORT;
    return rest.startsWith(":") ? "[" + host + "]" + rest : "";
  }
  const colons = (value.match(/:/g) ?? []).length;
  // 裸 IPv6 没有方括号时无法区分末尾数字是地址还是端口，统一按默认端口。
  if (colons >= 2) return "[" + value + "]:" + DEFAULT_PORT;
  return colons === 1 ? value : value + ":" + DEFAULT_PORT;
}

function isIPv6(host: string): boolean {
  if (!host.includes(":") || !/^[0-9a-fA-F:.]+$/.test(host)) return false;
  if (host.split("::").length > 2) return false;
  const groups = host.split(":").filter(Boolean);
  return groups.length <= 8 && groups.every((group) => /^[0-9a-fA-F]{1,4}$/.test(group));
}

function isHostnameOrIPv4(host: string): boolean {
  return /^(\d{1,3}(?:\.\d{1,3}){3}|[a-zA-Z0-9.-]+)$/.test(host);
}

/** 返回空串表示通过；否则是给用户看的错误文案。 */
export function validateDeviceAddress(address: string, code = ""): string {
  const normalized = normalizeDeviceAddress(address);
  if (!normalized) return "请填写平板上的 IP 地址、主机名或 IPv6 地址";

  let host = "";
  let portText = "";
  if (normalized.startsWith("[")) {
    const close = normalized.indexOf("]");
    if (close < 0 || !normalized.slice(close + 1).startsWith(":")) {
      return "地址格式应为 IP:端口、主机名:端口 或 [IPv6]:端口，例如 192.168.1.23:5555";
    }
    host = normalized.slice(1, close);
    portText = normalized.slice(close + 2);
    if (!isIPv6(host)) return "IPv6 地址格式不正确，请写成 [地址]:端口";
  } else {
    const match = /^(.+):(\d{1,5})$/.exec(normalized);
    if (!match) {
      return "地址格式应为 IP:端口、主机名:端口 或 [IPv6]:端口，例如 192.168.1.23:5555";
    }
    host = match[1];
    portText = match[2];
    if (!isHostnameOrIPv4(host)) return "主机名只能包含字母、数字、点和连字符";
  }

  if (!/^\d{1,5}$/.test(portText)) return "端口必须是 1–65535 之间的数字";
  const port = Number(portText);
  if (port < 1 || port > 65535) return "端口必须在 1–65535 之间";
  if (code && !/^\d{6}$/.test(code.trim())) return "配对码是平板上的 6 位数字";
  return "";
}
