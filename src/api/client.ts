/** 统一传输层。组件只认识 Backend 接口，不知道自己跑在 Tauri 还是浏览器里。 */

import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { openPath as tauriOpenPath } from '@tauri-apps/plugin-opener';
import { readText, writeText } from '@tauri-apps/plugin-clipboard-manager';
import { MockBackend, type EventHandler } from './mock';

export interface Backend {
  readonly isMock: boolean;
  call<T>(method: string, params?: Record<string, unknown>): Promise<T>;
  on(event: string, handler: EventHandler): () => void;
  clipboardRead(): Promise<string>;
  clipboardWrite(text: string): Promise<void>;
  windowMinimize(): Promise<void>;
  windowToggleMaximize(): Promise<void>;
  windowClose(): Promise<void>;
  openPath(path: string): Promise<void>;
  setCloseToTray(enabled: boolean): Promise<void>;
  setTrayState(captured: boolean, connected: boolean, device: string): Promise<void>;
  shutdown(): Promise<void>;
  quitApp(): Promise<void>;
}

function normalizeError(error: unknown): Error {
  if (error instanceof Error) return error;
  if (typeof error === 'string') return new Error(error);
  if (error && typeof error === 'object' && 'message' in error) {
    return new Error(String((error as { message: unknown }).message));
  }
  return new Error(String(error));
}

class TauriBackend implements Backend {
  readonly isMock = false;

  async call<T>(method: string, params: Record<string, unknown> = {}): Promise<T> {
    try {
      return await invoke<T>('rpc', { method, params });
    } catch (error) {
      throw normalizeError(error);
    }
  }

  on(event: string, handler: EventHandler): () => void {
    let unlisten: UnlistenFn | null = null;
    let cancelled = false;
    void listen(event, (payload) => handler(payload.payload))
      .then((fn) => {
        if (cancelled) fn();
        else unlisten = fn;
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }

  clipboardRead(): Promise<string> {
    return readText();
  }

  clipboardWrite(text: string): Promise<void> {
    return writeText(text);
  }

  async windowMinimize(): Promise<void> {
    await getCurrentWindow().minimize();
  }

  async windowToggleMaximize(): Promise<void> {
    await getCurrentWindow().toggleMaximize();
  }

  async windowClose(): Promise<void> {
    await getCurrentWindow().close();
  }

  async openPath(path: string): Promise<void> {
    await tauriOpenPath(path);
  }

  async setCloseToTray(enabled: boolean): Promise<void> {
    await invoke('set_close_to_tray', { enabled });
  }

  async setTrayState(captured: boolean, connected: boolean, device: string): Promise<void> {
    await invoke('set_tray_state', { captured, connected, device });
  }

  async shutdown(): Promise<void> {
    await this.call('app.shutdown').catch(() => undefined);
    await invoke('service_shutdown').catch(() => undefined);
  }

  async quitApp(): Promise<void> {
    await invoke('quit_app');
  }
}

function isTauriRuntime(): boolean {
  return (
    typeof window !== 'undefined' &&
    ('__TAURI_INTERNALS__' in window || '__TAURI__' in window)
  );
}

export const isTauri = isTauriRuntime();

export const backend: Backend = isTauri ? new TauriBackend() : new MockBackend();

/** 业务方法名集中在这里，避免字符串散落在各页面。 */
export const RPC = {
  bootstrap: 'app.bootstrap',
  shutdown: 'app.shutdown',
  configGet: 'config.get',
  configSave: 'config.save',
  devicesScan: 'devices.scan',
  devicesLocate: 'devices.locate',
  devicesConnect: 'devices.connect',
  devicesDisconnect: 'devices.disconnect',
  devicesCancel: 'devices.cancel',
  devicesAdd: 'devices.add',
  devicesRemove: 'devices.remove',
  layoutDesktop: 'layout.desktop',
  layoutInitial: 'layout.initial',
  layoutSave: 'layout.save',
  engineToggle: 'engine.toggle',
  enginePanic: 'engine.panic',
  engineReconnect: 'engine.reconnect',
  engineInjectClipboard: 'engine.inject_clipboard',
  engineState: 'engine.state',
  engineMetrics: 'engine.metrics',
  diagnosticsRun: 'diagnostics.run',
  diagnosticsExport: 'diagnostics.export',
  diagnosticsStatic: 'diagnostics.static',
  systemPaths: 'system.paths',
  wizardCheck: 'wizard.check',
  logsRecent: 'logs.recent',
} as const;
