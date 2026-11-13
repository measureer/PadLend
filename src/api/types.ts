/** 前后端契约：与 service/serializers.py 一一对应，禁止在各组件里另造形状。 */

export type Edge = 'left' | 'right' | 'top' | 'bottom';
export type Align = 'proportional' | 'center' | 'one_to_one';
export type Orientation = 'landscape' | 'portrait';
export type Theme = 'dark' | 'light';
export type PageId = 'devices' | 'layout' | 'settings' | 'logs';
export type AppState =
  | ''
  | 'disconnected'
  | 'connecting'
  | 'idle'
  | 'arming'
  | 'returning'
  | 'captured'
  | 'fault';

export interface Rect {
  left: number;
  top: number;
  right: number;
  bottom: number;
  width: number;
  height: number;
}

export interface Monitor {
  handle: number;
  device: string;
  name: string;
  rect: Rect;
  dpi: number;
  scale: number;
  primary: boolean;
}

/** 外轮廓边段，同时承载 Placement 的段字段（扁平 DTO）。 */
export interface Segment {
  monitorHandle: number;
  edge: Edge;
  spanStart: number;
  spanEnd: number;
  length: number;
  offset?: number;
  align?: Align;
  cornerPriority?: Edge | null;
}

export interface Placement {
  monitorHandle: number;
  edge: Edge;
  spanStart: number;
  spanEnd: number;
  offset: number;
  align: Align;
  cornerPriority: Edge | null;
}

export interface Desktop {
  monitors: Monitor[];
  segments: Segment[];
  boundingBox: Rect;
}

export interface DeviceFacts {
  serial: string;
  model: string;
  width: number;
  height: number;
  orientation: Orientation;
  xscale: number;
  pointerSpeed: number;
  hardwareSerial: string;
}

export interface DeviceItem {
  serial: string;
  /** 稳定设备 id（hw:… / ep:…）；换 IP、换端口后不变。 */
  id: string;
  state: string;
  model: string;
  transport: string;
  online: boolean;
  remembered: boolean;
  located: string;
  locatedDetail: string;
  /** 最后一次成功连接的 UTC ISO 时间；从未成功时为空。 */
  lastSeen: string;
}

export interface Metrics {
  raw_hz?: number;
  send_hz?: number;
  backlog?: number;
  drop_pct?: number;
  cpu_pct?: number;
  mem_mb?: number;
  rtt_ms?: number;
  p50_ms?: number;
  p95_ms?: number;
}

export interface LogEntry {
  timestamp: string;
  event: string;
  message: string;
}

export interface PlacementCfg {
  monitor: string;
  edge: Edge;
  span_start: number;
  span_end: number;
  offset: number;
  align: Align;
  corner_priority: string;
}

export interface Config {
  version: number;
  device: {
    serial: string;
    prefer_usb: boolean;
    auto_connect: boolean;
    server_port: number;
    known: string[];
    macs: Record<string, string>;
  };
  placement: PlacementCfg;
  layouts: { devices: Record<string, Partial<Record<Orientation, PlacementCfg>>> };
  motion: {
    gain: number;
    report_hz: number;
    hid_clamp: number;
    entry_margin_px: number;
    exit_overflow_px: number;
    debounce_ms: number;
    arm_px: number;
    arm_window_ms: number;
    edge_band_px: number;
    boundary_inset_px: number;
  };
  keyboard: {
    forward: boolean;
    suppress_pc: boolean;
    layout: string;
    passthrough_keys: string[];
  };
  hotkeys: { toggle: string; panic: string };
  ui: {
    language: string;
    theme: Theme;
    tray_notifications: boolean;
    close_to_tray: boolean;
  };
  misc: {
    clipboard_sync: boolean;
    keep_screen_on: boolean;
    restore_pointer_speed: number;
  };
}

export interface Bootstrap {
  version: string;
  config: Config;
  configIssues: string[];
  configPath: string;
  firstRun: boolean;
  simulated: boolean;
  theme: Theme;
  state: string;
  connected: boolean;
  facts: DeviceFacts | null;
  metrics: Metrics;
  desktop: Desktop;
  placement: Placement | null;
  devices: DeviceItem[];
  logs: LogEntry[];
}

export interface DiagnosticsResult {
  text: string;
  summary: string[];
  errors: string[];
  device: { serial: string; state: string };
  collectedAt: string;
}

export interface SystemPaths {
  configPath: string;
  logDir: string;
  noticesPath: string;
  version: string;
  license: string;
  appName: string;
}

export type DialogId = 'about' | 'diagnostics' | 'wizard' | 'addDevice' | null;

export interface ToastMessage {
  id: number;
  text: string;
  tone: 'ok' | 'warn' | 'err';
}
