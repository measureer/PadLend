/** 全应用唯一的事件订阅点：后端推送 → 精确更新对应 slice。
 * 组件不注册全局监听，也不轮询；这正是旧版「1s QTimer + 全量重建」的替代。 */

import { backend } from '../api/client';
import type { Bootstrap, Config, DeviceFacts, DeviceItem, LogEntry, Metrics, Placement } from '../api/types';
import * as actions from '../lib/actions';
import { makeLog } from '../lib/actions';
import { useAppStore } from './appStore';

const s = () => useAppStore.getState();

async function handleTrayAction(action: string): Promise<void> {
  switch (action) {
    case 'toggle':
      await actions.toggleManual();
      break;
    case 'reconnect':
      await actions.reconnect();
      break;
    case 'open-config':
      await actions.openPath('config');
      break;
    case 'copy-diagnostics':
      await actions.copyStaticDiagnostics();
      break;
    case 'open-logs':
      await actions.openPath('logs');
      break;
    case 'about':
      s().openDialog('about');
      break;
    case 'run-diagnostics':
      s().openDialog('diagnostics');
      void actions.runDiagnostics();
      break;
    case 'wizard':
      s().openDialog('wizard');
      break;
    case 'quit':
      await actions.quitApp();
      break;
    default:
      break;
  }
}

export function connectEvents(): () => void {
  const unsubscribers = [
    backend.on('ready', (payload) => {
      const data = payload as Bootstrap;
      if (!s().hydrated) {
        s().hydrate(data);
        actions.applyTheme(data.theme);
      }
    }),
    backend.on('state', (payload) => {
      const p = payload as { new: string };
      s().setState(p.new ?? '');
      void actions.syncTray();
    }),
    backend.on('metrics', (payload) => s().setMetrics(payload as Metrics)),
    backend.on('facts', (payload) => {
      s().setFacts((payload ?? null) as DeviceFacts | null);
      void actions.syncTray();
    }),
    backend.on('devices', (payload) => {
      const p = payload as { devices: DeviceItem[]; error?: string };
      s().setDevices(p.devices ?? [], p.error ?? '');
    }),
    backend.on('placement', (payload) => {
      const p = payload as { placement: Placement | null; dirty: boolean };
      s().setPlacement(p.placement, Boolean(p.dirty));
    }),
    backend.on('config', (payload) => {
      const p = payload as { config: Config; issues: string[] };
      s().setConfig(p.config, p.issues ?? []);
    }),
    backend.on('log', (payload) => s().appendLogs([payload as LogEntry])),
    backend.on('log:batch', (payload) => {
      const p = payload as { entries: LogEntry[] };
      s().appendLogs(p.entries ?? []);
    }),
    backend.on('clipboard', (payload) => {
      const p = payload as { text: string };
      void actions.noteTabletClipboard(p.text);
    }),
    backend.on('orientation', (payload) => {
      const facts = payload as DeviceFacts;
      s().appendLogs([makeLog('ORIENT', `平板旋转为${facts?.orientation === 'landscape' ? '横屏' : '竖屏'}`)]);
    }),
    backend.on('diagnostics:progress', (payload) => {
      const p = payload as { label: string };
      s().setDiagnostics({ progress: `${p.label} …` });
    }),
    backend.on('fatal', (payload) => {
      const p = payload as { message: string };
      s().pushToast(`业务服务异常：${p.message}`, 'err');
    }),
    backend.on('service:disconnected', () => {
      s().pushToast('业务服务已断开，请重启应用', 'err');
    }),
    backend.on('tray:action', (payload) => {
      const p = payload as { action: string };
      void handleTrayAction(p.action);
    }),
    backend.on('tray:minimized', () => s().pushToast('PadLend 已收进托盘，仍在后台运行')),
    backend.on('app:present', () => s().setPage('devices')),
  ];
  return () => unsubscribers.forEach((off) => off());
}
