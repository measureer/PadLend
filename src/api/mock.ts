/** 浏览器演示/测试用的内存后端，实现与 Tauri 后端完全相同的契约。
 *
 * 作用：无需 Rust 工具链、无需真实平板即可在浏览器里验证全部页面与交互；
 * 单元测试也用它作为确定性的事件源。生产路径走 client.ts 的 TauriBackend。
 */

import type {
  Bootstrap,
  Config,
  DeviceFacts,
  DeviceItem,
  LogEntry,
  Metrics,
  Placement,
} from './types';

export type EventHandler = (payload: unknown) => void;

export function defaultConfig(): Config {
  return {
    version: 1,
    device: {
      serial: '',
      prefer_usb: true,
      auto_connect: true,
      server_port: 1234,
      known: ['192.168.1.23:5555'],
      macs: { '192.168.1.23:5555': 'a4:ef:39:1c:22:08' },
    },
    placement: {
      monitor: 'DISPLAY2',
      edge: 'right',
      span_start: 0,
      span_end: 1080,
      offset: 0.5,
      align: 'proportional',
      corner_priority: 'right',
    },
    layouts: { devices: {} },
    motion: {
      gain: 1.0,
      report_hz: 1000,
      hid_clamp: 127,
      entry_margin_px: 2,
      exit_overflow_px: 80,
      debounce_ms: 250,
      arm_px: 10,
      arm_window_ms: 80,
      edge_band_px: 1,
      boundary_inset_px: 0,
    },
    keyboard: {
      forward: true,
      suppress_pc: true,
      layout: 'us',
      passthrough_keys: ['VK_LWIN', 'VK_RWIN'],
    },
    hotkeys: { toggle: 'Ctrl+Alt+S', panic: 'Ctrl+Alt+Q' },
    ui: {
      language: 'zh_CN',
      theme: 'dark',
      tray_notifications: true,
      close_to_tray: false,
    },
    misc: {
      clipboard_sync: true,
      keep_screen_on: true,
      restore_pointer_speed: -3,
      forward_audio: false,
    },
  };
}

const DESKTOP = {
  monitors: [
    {
      handle: 1,
      device: '\\\\.\\DISPLAY1',
      name: 'DISPLAY1',
      rect: { left: 0, top: 0, right: 2560, bottom: 1600, width: 2560, height: 1600 },
      dpi: 144,
      scale: 1.5,
      primary: true,
    },
    {
      handle: 2,
      device: '\\\\.\\DISPLAY2',
      name: 'DISPLAY2',
      rect: { left: 2560, top: 0, right: 4480, bottom: 1080, width: 1920, height: 1080 },
      dpi: 96,
      scale: 1,
      primary: false,
    },
  ],
  // 只列真正的外轮廓段（内部接缝不参与吸附），与 DesktopMap.exterior_segments() 语义一致。
  segments: [
    { monitorHandle: 1, edge: 'left' as const, spanStart: 0, spanEnd: 1600, length: 1600 },
    { monitorHandle: 1, edge: 'right' as const, spanStart: 1080, spanEnd: 1600, length: 520 },
    { monitorHandle: 1, edge: 'top' as const, spanStart: 0, spanEnd: 2560, length: 2560 },
    { monitorHandle: 1, edge: 'bottom' as const, spanStart: 0, spanEnd: 2560, length: 2560 },
    { monitorHandle: 2, edge: 'right' as const, spanStart: 0, spanEnd: 1080, length: 1080 },
    { monitorHandle: 2, edge: 'top' as const, spanStart: 2560, spanEnd: 4480, length: 1920 },
    { monitorHandle: 2, edge: 'bottom' as const, spanStart: 2560, spanEnd: 4480, length: 1920 },
  ],
  boundingBox: { left: 0, top: 0, right: 4480, bottom: 1600, width: 4480, height: 1600 },
};

const FACTS: DeviceFacts = {
  serial: 'R58M3C0ABCDE',
  model: 'HONOR MagicPad 3',
  width: 3200,
  height: 2136,
  orientation: 'landscape',
  xscale: 1.0,
  pointerSpeed: -3,
  hardwareSerial: 'R58M3C0ABCDE',
};

const INITIAL_DEVICES: DeviceItem[] = [
  {
    serial: 'R58M3C0ABCDE',
    id: 'hw:r58m3c0abcde',
    state: 'device',
    model: 'HONOR MagicPad 3',
    transport: 'USB',
    online: true,
    remembered: false,
    located: '',
    locatedDetail: '',
    lastSeen: '2026-11-13T08:00:00+00:00',
  },
  {
    serial: '192.168.1.23:5555',
    id: 'hw:hw-magicpad-3',
    state: 'offline',
    model: '上次连接的设备',
    transport: '无线',
    online: false,
    remembered: true,
    located: 'offline',
    locatedDetail: '上次连接，当前未接入',
    lastSeen: '2026-11-12T09:30:00+00:00',
  },
];

function stamp(): string {
  const now = new Date();
  return (
    now.toTimeString().slice(0, 8) + '.' + String(now.getMilliseconds()).padStart(3, '0')
  );
}

const delay = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

export class MockBackend {
  readonly isMock = true;
  private listeners = new Map<string, Set<EventHandler>>();
  private config = defaultConfig();
  private devices = INITIAL_DEVICES.map((d) => ({ ...d }));
  private facts: DeviceFacts | null = null;
  private state = 'idle';
  private metrics: Metrics = {
    raw_hz: 126.4,
    send_hz: 125.8,
    backlog: 0,
    drop_pct: 0.02,
    cpu_pct: 1.8,
    mem_mb: 86.4,
    rtt_ms: 4.2,
    p50_ms: 3.1,
    p95_ms: 7.9,
  };
  private logs: LogEntry[] = [];
  private placement: Placement = {
    monitorHandle: 2,
    edge: 'right',
    spanStart: 0,
    spanEnd: 1080,
    offset: 0.5,
    align: 'proportional',
    cornerPriority: 'right',
  };
  private diagnosticsText = '';
  private timer: ReturnType<typeof setInterval> | null = null;

  constructor() {
    this.pushLog('APP', 'PadLend 1.5.0 业务服务已就绪（演示模式）');
    this.pushLog('DEVICE', '扫描设备：1 台设备');
    this.pushLog('STATE', 'idle → idle（启动）');
  }

  startTimers(): void {
    if (this.timer) return;
    this.timer = setInterval(() => {
      this.metrics = {
        ...this.metrics,
        raw_hz: 124 + Math.random() * 6,
        send_hz: 123 + Math.random() * 6,
        cpu_pct: 1 + Math.random() * 2.5,
        rtt_ms: 3 + Math.random() * 3,
      };
      this.emit('metrics', this.metrics);
    }, 2000);
  }

  on(event: string, handler: EventHandler): () => void {
    if (!this.listeners.has(event)) this.listeners.set(event, new Set());
    this.listeners.get(event)!.add(handler);
    this.startTimers();
    return () => this.listeners.get(event)?.delete(handler);
  }

  private emit(event: string, payload: unknown): void {
    for (const handler of this.listeners.get(event) ?? []) {
      Promise.resolve().then(() => handler(payload));
    }
  }

  private pushLog(event: string, message: string): LogEntry {
    const entry = { timestamp: stamp(), event, message };
    this.logs.push(entry);
    if (this.logs.length > 2000) this.logs.splice(0, this.logs.length - 2000);
    this.emit('log', entry);
    return entry;
  }

  private setState(next: string, reason: string): void {
    const old = this.state;
    this.state = next;
    this.emit('state', { old, new: next, reason });
    this.pushLog('STATE', `${old} → ${next}（${reason}）`);
  }

  private snapshot(): Bootstrap {
    return {
      version: '1.5.0',
      config: this.config,
      configIssues: [],
      configPath: '%APPDATA%\\PadLend\\config.yaml',
      firstRun: false,
      simulated: true,
      theme: this.config.ui.theme,
      state: this.state,
      connected: this.facts !== null,
      facts: this.facts,
      metrics: this.metrics,
      desktop: DESKTOP,
      placement: this.placement,
      devices: this.devices,
      logs: this.logs.slice(-200),
    };
  }

  async call<T = unknown>(method: string, params: Record<string, unknown> = {}): Promise<T> {
    await delay(method.startsWith('devices.connect') ? 420 : 90 + Math.random() * 80);
    let result: unknown;
    switch (method) {
      case 'app.bootstrap':
        result = this.snapshot();
        break;
      case 'config.get':
        result = { config: this.config, issues: [], path: '%APPDATA%\\PadLend\\config.yaml' };
        break;
      case 'config.save': {
        const patch = (params.patch ?? {}) as Partial<Config>;
        this.config = {
          ...this.config,
          ...patch,
          motion: { ...this.config.motion, ...(patch.motion ?? {}) },
          keyboard: { ...this.config.keyboard, ...(patch.keyboard ?? {}) },
          hotkeys: { ...this.config.hotkeys, ...(patch.hotkeys ?? {}) },
          ui: { ...this.config.ui, ...(patch.ui ?? {}) },
          misc: { ...this.config.misc, ...(patch.misc ?? {}) },
          device: { ...this.config.device, ...(patch.device ?? {}) },
        };
        this.emit('config', { config: this.config, issues: [] });
        this.pushLog('CONFIG', '配置已保存：%APPDATA%\\PadLend\\config.yaml');
        result = { config: this.config, issues: [], path: '%APPDATA%\\PadLend\\config.yaml' };
        break;
      }
      case 'devices.scan':
        this.emit('devices', { devices: this.devices, error: '' });
        this.pushLog('DEVICE', '扫描设备：1 台设备');
        result = { devices: this.devices, hint: '1 台设备', error: '' };
        break;
      case 'devices.locate':
        result = { located: {} };
        break;
      case 'devices.connect': {
        this.setState('connecting', 'device.connect');
        await delay(500);
        this.facts = { ...FACTS };
        this.setState('idle', 'tablet.connect');
        this.emit('facts', this.facts);
        this.pushLog('DEVICE', `已连接 ${this.facts.serial}`);
        result = { ok: true, message: '' };
        break;
      }
      case 'devices.disconnect':
        this.facts = null;
        this.setState('idle', 'device.disconnect');
        this.emit('facts', null);
        this.pushLog('DEVICE', '已断开');
        result = { ok: true };
        break;
      case 'devices.add': {
        const address = String(params.address ?? '');
        this.devices = [
          ...this.devices,
          { serial: address, id: 'ep:' + address.toLowerCase(), state: 'device', model: '无线平板', transport: '无线', online: true, remembered: false, located: '', locatedDetail: '', lastSeen: '2026-11-13T00:00:00+00:00' },
        ];
        this.emit('devices', { devices: this.devices, error: '' });
        result = { ok: true, message: '' };
        break;
      }
      case 'devices.remove': {
        const serial = String(params.serial ?? '');
        this.devices = this.devices.filter((d) => d.serial !== serial);
        this.emit('devices', { devices: this.devices, error: '' });
        this.pushLog('DEVICE', `已从列表移除 ${serial}`);
        result = { ok: true, config: this.config };
        break;
      }
      case 'layout.desktop':
        result = { ...DESKTOP, placement: this.placement, tablet: this.facts };
        break;
      case 'layout.initial':
        result = { placement: this.placement };
        break;
      case 'layout.save': {
        this.placement = params.placement as Placement;
        this.config = {
          ...this.config,
          placement: {
            ...this.config.placement,
            edge: this.placement.edge,
            span_start: this.placement.spanStart,
            span_end: this.placement.spanEnd,
            offset: this.placement.offset,
            align: this.placement.align,
            corner_priority: this.placement.cornerPriority ?? '',
          },
        };
        this.emit('placement', { placement: this.placement, dirty: false, reason: 'saved' });
        this.emit('config', { config: this.config, issues: [] });
        this.pushLog('PLACE', `布局已保存：吸附 ${this.placement.edge} offset=${this.placement.offset.toFixed(2)}`);
        result = { config: this.config, placement: this.placement };
        break;
      }
      case 'engine.toggle':
        this.setState(this.state === 'captured' ? 'idle' : 'captured', 'manual.toggle');
        this.pushLog('MANUAL', '手动接管 / 交还');
        result = { ok: true };
        break;
      case 'engine.panic':
        this.setState('idle', 'panic');
        this.pushLog('PANIC', '急停：已释放遮罩与输入');
        result = { ok: true };
        break;
      case 'engine.reconnect':
        this.pushLog('RECONNECT', '重新同步指针');
        result = { ok: true };
        break;
      case 'engine.inject_clipboard':
        result = { ok: Boolean(params.text) };
        break;
      case 'engine.state':
        result = { state: this.state, connected: this.facts !== null, facts: this.facts, metrics: this.metrics, manual: false };
        break;
      case 'engine.metrics':
        result = this.metrics;
        break;
      case 'diagnostics.run': {
        this.diagnosticsText = [
          'PadLend 1.5.0 诊断报告',
          '（设备序列号、IP/MAC、用户名与用户目录路径已自动打码）',
          '',
          '== 概要 ==',
          '设备：<serial>（device）',
          'XScale：1.0',
          'pointer_speed：-3',
          'UHID 组：有',
          'UHID 可打开：是',
          'server jar：已就绪',
          '',
          '== adb devices -l ==',
          '<serial>\tdevice usb:1-1 product:p3 model:PadLend_Demo',
        ].join('\n');
        result = { text: this.diagnosticsText, summary: [], errors: [], device: { serial: '<serial>', state: 'device' }, collectedAt: new Date().toISOString() };
        break;
      }
      case 'diagnostics.export':
        result = { path: 'C:\\Users\\<user>\\Desktop\\padlend-diagnostics-demo.zip' };
        break;
      case 'diagnostics.static':
        result = { text: 'PadLend 1.5.0 · Windows · jar 已就绪（未运行 adb 采集）' };
        break;
      case 'system.paths':
        result = {
          configPath: 'C:\\Users\\<user>\\AppData\\Roaming\\PadLend\\config.yaml',
          logDir: 'C:\\Users\\<user>\\AppData\\Roaming\\PadLend\\logs',
          noticesPath: 'C:\\Program Files\\PadLend\\THIRD_PARTY_NOTICES.md',
          version: '1.5.0',
          license: 'Apache-2.0',
          appName: 'PadLend',
        };
        break;
      case 'wizard.check': {
        const step = Number(params.step ?? 0);
        const texts = [
          '已连接：R58M3C0ABCDE；USB 调试已开启。',
          'uhid 组：有\n/dev/uhid 可打开：是\n相对鼠标注入可用。',
          '一切就绪，回到主窗口开始使用。',
        ];
        result = { ok: true, text: texts[step] ?? texts[2] };
        break;
      }
      case 'logs.recent':
        result = { logs: this.logs.slice(-Number(params.limit ?? 200)) };
        break;
      case 'app.shutdown':
        result = { ok: true };
        break;
      default:
        throw new Error(`演示后端未实现方法：${method}`);
    }
    return result as T;
  }

  async clipboardRead(): Promise<string> {
    try {
      return await navigator.clipboard.readText();
    } catch {
      return '';
    }
  }

  async clipboardWrite(text: string): Promise<void> {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      const area = document.createElement('textarea');
      area.value = text;
      document.body.appendChild(area);
      area.select();
      document.execCommand('copy');
      area.remove();
    }
  }

  // 浏览器预览里窗口控制/托盘是空操作，接口保持一致。
  async windowMinimize(): Promise<void> {}
  async windowToggleMaximize(): Promise<void> {}
  async windowClose(): Promise<void> {}
  async openPath(): Promise<void> {}
  async setCloseToTray(): Promise<void> {}
  async setTrayState(): Promise<void> {}
  async shutdown(): Promise<void> {}
  async quitApp(): Promise<void> {}
}
