import { beforeEach, describe, expect, it, vi } from 'vitest';
import { defaultConfig } from '../api/mock';
import { useAppStore } from '../store/appStore';

const mocks = vi.hoisted(() => ({
  call: vi.fn(),
  setCloseToTray: vi.fn().mockResolvedValue(undefined),
  setTrayState: vi.fn().mockResolvedValue(undefined),
}));

vi.mock('../api/client', () => ({
  backend: {
    call: mocks.call,
    setCloseToTray: mocks.setCloseToTray,
    setTrayState: mocks.setTrayState,
    isMock: false,
  },
  isTauri: false,
  RPC: {
    bootstrap: 'app.bootstrap',
    systemPaths: 'system.paths',
    devicesScan: 'devices.scan',
    devicesLocate: 'devices.locate',
    devicesConnect: 'devices.connect',
    devicesCancel: 'devices.cancel',
    configSave: 'config.save',
    layoutSave: 'layout.save',
    layoutInitial: 'layout.initial',
  },
}));

import {
  bootstrap,
  cancelConnect,
  connectDevice,
  refreshDevices,
  removeLayout,
  saveConfig,
  toggleTheme,
} from '../lib/actions';

function resetStore(): void {
  useAppStore.setState({
    config: defaultConfig(),
    configIssues: [],
    configPath: '',
    configBanner: null,
    toasts: [],
    busy: {},
    scanHint: '',
    deviceError: '',
    devices: [],
    selectedSerial: '',
    logs: [],
    state: 'idle',
    connected: false,
    facts: null,
    theme: 'dark',
    placement: null,
    layoutDirty: false,
  });
}

describe('动作层错误分支', () => {
  beforeEach(() => {
    mocks.call.mockReset();
    mocks.setCloseToTray.mockClear();
    mocks.setTrayState.mockClear();
    resetStore();
  });

  it('bootstrap 失败时给出业务服务连接失败提示', async () => {
    mocks.call.mockRejectedValueOnce(new Error('socket closed'));

    await bootstrap();

    const state = useAppStore.getState();
    expect(state.toasts).toHaveLength(1);
    expect(state.toasts[0].tone).toBe('err');
    expect(state.toasts[0].text).toBe('业务服务连接失败：socket closed');
  });

  it('刷新设备失败时保留错误并清除扫描 busy', async () => {
    mocks.call.mockRejectedValueOnce(new Error('adb missing'));

    await refreshDevices();

    const state = useAppStore.getState();
    expect(state.scanHint).toBe('扫描失败');
    expect(state.deviceError).toBe('adb missing');
    expect(state.busy.scan).toBe(false);
  });

  it('连接失败时提示错误并清除连接 busy', async () => {
    mocks.call.mockRejectedValueOnce(new Error('unauthorized'));

    await connectDevice('192.0.2.10:5555');

    const state = useAppStore.getState();
    expect(state.deviceError).toBe('unauthorized');
    expect(state.busy.connect).toBe(false);
    expect(state.toasts[state.toasts.length - 1]).toMatchObject({
      text: 'unauthorized',
      tone: 'err',
    });
    expect(state.logs[state.logs.length - 1].message).toContain('192.0.2.10:5555');
  });

  it('取消连接后给出提示，并把连接 busy 交给原请求收尾', async () => {
    mocks.call.mockResolvedValueOnce({ ok: true });

    await cancelConnect();

    const state = useAppStore.getState();
    expect(mocks.call).toHaveBeenCalledWith('devices.cancel');
    expect(state.deviceError).toBe('连接已取消');
    expect(state.toasts[state.toasts.length - 1]).toMatchObject({ text: '已取消连接' });
    expect(state.logs[state.logs.length - 1].message).toBe('取消连接');
  });

  it('主题切换保存失败时回滚 store 与 DOM 主题', async () => {
    useAppStore.setState({ theme: 'dark' });
    document.documentElement.dataset.theme = 'dark';
    mocks.call.mockRejectedValueOnce(new Error('disk full'));

    await toggleTheme();

    expect(mocks.call).toHaveBeenCalledWith('config.save', expect.objectContaining({
      patch: expect.objectContaining({ ui: expect.objectContaining({ theme: 'light' }) }),
    }));
    expect(useAppStore.getState().theme).toBe('dark');
    expect(document.documentElement.dataset.theme).toBe('dark');
    expect(useAppStore.getState().toasts[useAppStore.getState().toasts.length - 1]).toMatchObject({
      text: '主题未保存，已恢复原主题',
      tone: 'warn',
    });
  });

  it('主题切换成功时保留新主题', async () => {
    useAppStore.setState({ theme: 'dark' });
    document.documentElement.dataset.theme = 'dark';
    const config = defaultConfig();
    mocks.call.mockResolvedValueOnce({
      config: { ...config, ui: { ...config.ui, theme: 'light' } },
      issues: [],
      path: 'C:/tmp/config.yaml',
    });

    await toggleTheme();

    expect(useAppStore.getState().theme).toBe('light');
    expect(document.documentElement.dataset.theme).toBe('light');
    expect(useAppStore.getState().config.ui.theme).toBe('light');
  });

  it('移除布局走 layout.save 的 remove 契约并回填引擎返回值', async () => {
    const config = defaultConfig();
    const fallback = {
      monitorHandle: 1, edge: 'left' as const, spanStart: 0, spanEnd: 1080,
      offset: 0.5, align: 'proportional' as const, cornerPriority: null,
    };
    useAppStore.setState({ layoutDirty: true });
    mocks.call.mockResolvedValueOnce({ ok: true, removed: true, config, placement: fallback });

    await removeLayout('R58M3C0ABCDE', 'landscape');

    expect(mocks.call).toHaveBeenCalledWith('layout.save', {
      identity: 'R58M3C0ABCDE',
      orientation: 'landscape',
      remove: true,
    });
    const state = useAppStore.getState();
    expect(state.config).toBe(config);
    expect(state.placement).toEqual(fallback);
    expect(state.layoutDirty).toBe(false);
    expect(state.toasts[state.toasts.length - 1]).toMatchObject({
      text: '已移除该方向布局，回退到全局摆位',
      tone: 'ok',
    });
  });

  it('当前方向没有自定义布局时只提示，不清掉预览', async () => {
    const config = defaultConfig();
    const current = {
      monitorHandle: 2, edge: 'right' as const, spanStart: 0, spanEnd: 1080,
      offset: 0.5, align: 'proportional' as const, cornerPriority: 'right' as const,
    };
    useAppStore.setState({ placement: current, layoutDirty: false });
    mocks.call
      .mockResolvedValueOnce({ ok: true, removed: false, config })
      .mockResolvedValueOnce({ placement: current });

    await removeLayout('R58M3C0ABCDE', 'portrait');

    const state = useAppStore.getState();
    expect(mocks.call).toHaveBeenLastCalledWith('layout.initial');
    expect(state.placement).toEqual(current);
    expect(state.layoutDirty).toBe(false);
    expect(state.toasts[state.toasts.length - 1]).toMatchObject({
      text: '当前方向没有自定义布局',
      tone: 'warn',
    });
  });

  it('移除布局失败时保留预览并提示错误', async () => {
    const current = {
      monitorHandle: 2, edge: 'right' as const, spanStart: 0, spanEnd: 1080,
      offset: 0.5, align: 'proportional' as const, cornerPriority: null,
    };
    useAppStore.setState({ placement: current });
    mocks.call.mockRejectedValueOnce(new Error('engine down'));

    await removeLayout('R58M3C0ABCDE', 'landscape');

    expect(useAppStore.getState().placement).toEqual(current);
    expect(useAppStore.getState().toasts[useAppStore.getState().toasts.length - 1]).toMatchObject({
      text: 'engine down',
      tone: 'err',
    });
  });

  it('保存配置失败时返回 false，配置不变并显示错误横幅', async () => {
    const before = useAppStore.getState().config;
    mocks.call.mockRejectedValueOnce(new Error('disk full'));

    const ok = await saveConfig({ ui: { ...before.ui, theme: 'light' } }, '主题已切换');

    const state = useAppStore.getState();
    expect(ok).toBe(false);
    expect(state.config).toBe(before);
    expect(state.configBanner).toEqual({ tone: 'err', text: 'disk full' });
    expect(state.toasts[state.toasts.length - 1]).toMatchObject({
      text: 'disk full',
      tone: 'err',
    });
  });
});
