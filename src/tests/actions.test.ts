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
  },
}));

import { bootstrap, cancelConnect, connectDevice, refreshDevices, saveConfig } from '../lib/actions';

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
