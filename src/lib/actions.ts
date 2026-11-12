/** 界面动作编排层：组件只调用这里的语义化函数，不直接写 IPC 字符串。 */

import { backend, isTauri, RPC } from '../api/client';
import type {
  Bootstrap,
  Config,
  DeviceItem,
  DiagnosticsResult,
  LogEntry,
  Placement,
  SystemPaths,
  Theme,
} from '../api/types';
import { useAppStore } from '../store/appStore';

const store = () => useAppStore.getState();

export function applyTheme(theme: Theme): void {
  document.documentElement.dataset.theme = theme;
  useAppStore.getState().setTheme(theme);
}

let tabletClipboardSuppressUntil = 0;

export async function noteTabletClipboard(text: string): Promise<void> {
  if (!text || !store().config.misc.clipboard_sync) return;
  tabletClipboardSuppressUntil = Date.now() + 1000;
  await backend.clipboardWrite(text).catch(() => undefined);
  store().pushToast('平板剪贴板已同步到 PC');
}

/** 只在接管期间、且剪贴板内容真的变化时才发请求（500ms 轮询 + 去重）。 */
export async function tickClipboardSync(): Promise<boolean> {
  const s = store();
  if (!s.config.misc.clipboard_sync || s.state !== 'captured') return false;
  if (Date.now() < tabletClipboardSuppressUntil) return false;
  const text = await backend.clipboardRead().catch(() => '');
  if (!text || text === lastPcClipboard) return false;
  lastPcClipboard = text;
  await backend.call(RPC.engineInjectClipboard, { text }).catch(() => undefined);
  return true;
}

let lastPcClipboard = '';

export async function bootstrap(): Promise<void> {
  try {
    const data = await backend.call<Bootstrap>(RPC.bootstrap);
    store().hydrate(data);
    applyTheme(data.theme);
    const paths = await backend.call<SystemPaths>(RPC.systemPaths).catch(() => store().paths);
    store().setPaths(paths);
    await backend.setCloseToTray(data.config.ui.close_to_tray).catch(() => undefined);
    await syncTray();
    // 浏览器演示模式支持 ?page=layout&theme=light，方便直接预览各页面与主题；
    // Tauri 运行时仍以 config.ui.theme 与默认设备页为准。
    if (backend.isMock) {
      const query = new URLSearchParams(window.location.search);
      const page = query.get('page');
      if (page === 'layout' || page === 'settings' || page === 'logs' || page === 'devices') {
        store().setPage(page);
      }
      const theme = query.get('theme');
      if (theme === 'light' || theme === 'dark') applyTheme(theme);
    }
    if (data.firstRun) store().openDialog('wizard');
    // 扫描与局域网查找并行、不阻塞首屏渲染。
    void refreshDevices();
    void locateDevices();
  } catch (error) {
    store().pushToast(`业务服务连接失败：${(error as Error).message}`, 'err');
  }
}

export async function syncTray(): Promise<void> {
  const s = store();
  const captured = s.state === 'captured';
  await backend
    .setTrayState(captured, s.connected, s.facts?.serial ?? '')
    .catch(() => undefined);
}

export async function refreshDevices(): Promise<void> {
  store().setBusy('scan', true);
  try {
    const result = await backend.call<{ devices: DeviceItem[]; hint: string; error: string }>(RPC.devicesScan);
    store().setDevices(result.devices, result.error);
    store().setScanHint(result.hint);
  } catch (error) {
    store().setScanHint('扫描失败');
    store().setDeviceError((error as Error).message);
  } finally {
    store().setBusy('scan', false);
  }
}

export async function locateDevices(): Promise<void> {
  try {
    await backend.call(RPC.devicesLocate);
  } catch {
    // 局域网查找是尽力而为，失败保持原列表。
  }
}

export async function connectDevice(serial: string): Promise<void> {
  if (!serial) {
    store().setDeviceError('请选择一个已授权的设备');
    return;
  }
  store().setDeviceError('');
  store().setBusy('connect', true);
  store().appendLogs([makeLog('DEVICE', `连接请求：${serial}`)]);
  try {
    const result = await backend.call<{ ok: boolean; message: string }>(RPC.devicesConnect, { serial });
    if (!result.ok) {
      store().setDeviceError(result.message);
      store().pushToast(result.message, 'err');
    } else {
      store().pushToast(`已连接 ${serial}`);
    }
    void refreshDevices();
  } catch (error) {
    const message = (error as Error).message;
    store().setDeviceError(message);
    store().pushToast(message, 'err');
  } finally {
    store().setBusy('connect', false);
  }
}

export async function disconnectDevice(): Promise<void> {
  store().setBusy('connect', true);
  store().appendLogs([makeLog('DEVICE', '断开当前设备')]);
  try {
    await backend.call(RPC.devicesDisconnect);
    store().pushToast('已断开平板');
    void refreshDevices();
  } catch (error) {
    store().pushToast((error as Error).message, 'err');
  } finally {
    store().setBusy('connect', false);
  }
}

export async function addDevice(address: string, code: string): Promise<void> {
  store().setBusy('add', true);
  try {
    const result = await backend.call<{ ok: boolean; message: string }>(RPC.devicesAdd, { address, code });
    if (!result.ok) {
      store().pushToast(result.message || '添加失败', 'err');
      return;
    }
    store().openDialog(null);
    store().pushToast('无线设备已添加');
    void refreshDevices();
  } catch (error) {
    store().pushToast((error as Error).message, 'err');
  } finally {
    store().setBusy('add', false);
  }
}

export async function forgetDevice(serial: string): Promise<void> {
  try {
    await backend.call(RPC.devicesRemove, { serial });
    void refreshDevices();
  } catch (error) {
    store().pushToast((error as Error).message, 'err');
  }
}

export async function saveLayout(placement: Placement): Promise<void> {
  const s = store();
  const identity = s.facts?.hardwareSerial || s.facts?.serial || '';
  const orientation = s.facts?.orientation || '';
  try {
    const result = await backend.call<{ config: Config; placement: Placement }>(RPC.layoutSave, {
      placement,
      identity,
      orientation,
    });
    s.setConfig(result.config);
    s.setPlacement(result.placement, false);
    s.pushToast('布局已保存，立即生效');
  } catch (error) {
    s.pushToast((error as Error).message, 'err');
  }
}

export async function saveConfig(patch: Partial<Config>, successText = '设置已保存'): Promise<boolean> {
  const s = store();
  try {
    const result = await backend.call<{ config: Config; issues: string[]; path: string }>(RPC.configSave, { patch });
    s.setConfig(result.config, result.issues);
    s.setConfigBanner({ tone: 'ok', text: `${successText} · ${result.path}` });
    s.pushToast(successText);
    await backend.setCloseToTray(result.config.ui.close_to_tray).catch(() => undefined);
    return true;
  } catch (error) {
    const message = (error as Error).message;
    s.setConfigBanner({ tone: 'err', text: message });
    s.pushToast(message, 'err');
    return false;
  }
}

export async function toggleTheme(): Promise<void> {
  const s = store();
  const next: Theme = s.theme === 'dark' ? 'light' : 'dark';
  applyTheme(next); // CSS 变量切换，不需要重渲染任何业务组件
  await saveConfig({ ui: { ...s.config.ui, theme: next } }, '主题已切换');
}

export async function toggleManual(): Promise<void> {
  try {
    await backend.call(RPC.engineToggle);
  } catch (error) {
    store().pushToast((error as Error).message, 'err');
  }
}

export async function panic(): Promise<void> {
  try {
    await backend.call(RPC.enginePanic);
    store().pushToast('已急停，输入交还系统', 'warn');
  } catch (error) {
    store().pushToast((error as Error).message, 'err');
  }
}

export async function reconnect(): Promise<void> {
  try {
    await backend.call(RPC.engineReconnect);
    store().pushToast('指针已重新同步');
  } catch (error) {
    store().pushToast((error as Error).message, 'err');
  }
}

export async function runDiagnostics(): Promise<void> {
  const s = store();
  s.setDiagnostics({ collecting: true, status: '正在采集 …', progress: '' });
  try {
    const result = await backend.call<DiagnosticsResult>(RPC.diagnosticsRun);
    s.setDiagnostics({
      collecting: false,
      status: `采集完成 · ${result.device.serial || '未检测到设备'}`,
      text: result.text,
      progress: '',
    });
  } catch (error) {
    s.setDiagnostics({ collecting: false, status: `采集失败：${(error as Error).message}` });
  }
}

export async function exportDiagnostics(): Promise<void> {
  try {
    const result = await backend.call<{ path: string }>(RPC.diagnosticsExport);
    store().setDiagnostics({ status: `已导出：${result.path}` });
    store().pushToast('诊断包已导出到桌面');
  } catch (error) {
    store().pushToast((error as Error).message, 'err');
  }
}

export async function copyStaticDiagnostics(): Promise<void> {
  try {
    const result = await backend.call<{ text: string }>(RPC.diagnosticsStatic);
    await backend.clipboardWrite(result.text);
    store().pushToast('诊断摘要已复制');
  } catch (error) {
    store().pushToast((error as Error).message, 'err');
  }
}

export async function openPath(kind: 'config' | 'logs' | 'notices'): Promise<void> {
  const paths = store().paths;
  const target = kind === 'config' ? paths.configPath : kind === 'logs' ? paths.logDir : paths.noticesPath;
  if (!isTauri || !target) {
    store().pushToast(`路径：${target || '（演示模式）'}`, 'warn');
    return;
  }
  try {
    await backend.openPath(target);
  } catch (error) {
    store().pushToast(`无法打开：${(error as Error).message}`, 'err');
  }
}

export async function wizardCheck(step: number): Promise<{ ok: boolean; text: string }> {
  return backend.call<{ ok: boolean; text: string }>(RPC.wizardCheck, { step });
}

export async function wizardPushJar(): Promise<{ ok: boolean; text: string }> {
  return backend.call<{ ok: boolean; text: string }>(RPC.wizardPushJar);
}

export async function wizardFinish(autoConnect: boolean, closeToTray: boolean): Promise<void> {
  const ok = await saveConfig(
    {
      device: { ...store().config.device, auto_connect: autoConnect },
      ui: { ...store().config.ui, close_to_tray: closeToTray },
    },
    '首次设置已完成',
  );
  if (ok) store().openDialog(null);
}

export async function quitApp(): Promise<void> {
  await backend.shutdown().catch(() => undefined);
  // 不能用 windowClose()：close_to_tray=true 时它会被 prevent_close 拦下，
  // 只隐藏窗口，进程/托盘/遮罩都还在（缺陷 B）。
  if (isTauri) await backend.quitApp().catch(() => undefined);
}

export async function loadRecentLogs(): Promise<void> {
  try {
    const result = await backend.call<{ logs: LogEntry[] }>(RPC.logsRecent, { limit: 200 });
    store().appendLogs(result.logs);
  } catch {
    // 日志是旁路信息，加载失败不影响主流程。
  }
}

export function makeLog(event: string, message: string): LogEntry {
  const now = new Date();
  return {
    timestamp: now.toTimeString().slice(0, 8) + '.' + String(now.getMilliseconds()).padStart(3, '0'),
    event,
    message,
  };
}
