import { beforeEach, describe, expect, it } from 'vitest';
import { defaultConfig } from '../api/mock';
import type { Bootstrap, DeviceItem, LogEntry } from '../api/types';
import { MAX_LOGS, useAppStore } from '../store/appStore';

function bootstrap(): Bootstrap {
  return {
    version: '1.0.5',
    config: defaultConfig(),
    configIssues: [],
    configPath: 'C:/tmp/config.yaml',
    firstRun: false,
    simulated: true,
    theme: 'dark',
    state: 'idle',
    connected: false,
    facts: null,
    metrics: {},
    desktop: { monitors: [], segments: [], boundingBox: { left: 0, top: 0, right: 1920, bottom: 1080, width: 1920, height: 1080 } },
    placement: null,
    devices: [],
    logs: [],
  };
}

const device = (serial: string, online: boolean, id = serial): DeviceItem => ({
  serial, id, state: online ? 'device' : 'offline', model: serial, transport: 'USB',
  online, remembered: !online, located: '', locatedDetail: '', lastSeen: '',
});

const log = (index: number): LogEntry => ({ timestamp: '12:00:00.000', event: 'APP', message: `line ${index}` });

describe('应用状态切片', () => {
  beforeEach(() => {
    useAppStore.setState({ hydrated: false, logs: [], devices: [], selectedSerial: '', layoutDirty: false });
  });

  it('bootstrap 一次性灌入配置/桌面/日志', () => {
    useAppStore.getState().hydrate(bootstrap());
    const state = useAppStore.getState();
    expect(state.hydrated).toBe(true);
    expect(state.config.motion.report_hz).toBe(1000);
    expect(state.configPath).toContain('config.yaml');
  });

  it('日志上限 2000 行且不修改原数组', () => {
    const entries = Array.from({ length: 2050 }, (_, i) => log(i));
    useAppStore.getState().appendLogs(entries);
    const { logs } = useAppStore.getState();
    expect(logs).toHaveLength(MAX_LOGS);
    expect(logs[0].message).toBe('line 50');
  });

  it('设备列表变化时保留有效选择，否则选第一台在线设备', () => {
    const store = useAppStore.getState();
    store.setDevices([device('A', false), device('B', true)]);
    expect(useAppStore.getState().selectedSerial).toBe('B');
    store.setDevices([device('A', false)]);
    expect(useAppStore.getState().selectedSerial).toBe('A');
  });

  it('设备换端点后按稳定 id 保留选择', () => {
    const store = useAppStore.getState();
    store.setDevices([device('192.168.1.9:5555', true, 'hw:1'), device('USB2', true, 'hw:2')]);
    store.setSelectedSerial('192.168.1.9:5555');
    store.setDevices([device('10.0.0.9:37123', true, 'hw:1'), device('USB2', true, 'hw:2')]);
    expect(useAppStore.getState().selectedSerial).toBe('10.0.0.9:37123');
  });

  it('摆位预览标记 dirty，服务确认后清除', () => {
    const store = useAppStore.getState();
    const placement = { monitorHandle: 1, edge: 'right' as const, spanStart: 0, spanEnd: 100, offset: 0.5, align: 'proportional' as const, cornerPriority: null };
    store.setPlacement(placement);
    expect(useAppStore.getState().layoutDirty).toBe(true);
    useAppStore.getState().setPlacement(placement, false);
    expect(useAppStore.getState().layoutDirty).toBe(false);
  });
});
