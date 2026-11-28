/** 前后端契约：与 padlend-core/src/service/serializers.rs 一一对应，禁止在各组件里另造形状（历史：原 service/serializers.py）。 */

export type Edge = 'left' | 'right' | 'top' | 'bottom';
export type Orientation = 'landscape' | 'portrait';
export type Theme = 'dark' | 'light';
export type PageId = 'devices' | 'layout' | 'settings' | 'logs' | 'about';
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
  /** 显示缩放（0.1–4.0）：平板在桌面上占多大，不改设备像素。 */
  scale?: number;
  cornerPriority?: Edge | null;
}

export interface Placement {
  monitorHandle: number;
  edge: Edge;
  spanStart: number;
  spanEnd: number;
  /** 边段像素长度；引擎 placement_dict 总是下发（serializers.rs），前端回传时被 placement_from_dict 忽略。 */
  length: number;
  offset: number;
  /** 显示缩放（0.1–4.0）：平板在桌面上占多大，不改设备像素。 */
  scale: number;
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
  /** 最后一次成功连接的本地时间（ISO 秒，无时区后缀）；从未成功时为空。 */
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
  /** 前端本地自增序号（虚拟列表 key）；引擎不下发，由 store 追加。 */
  seq?: number;
}

export interface PlacementCfg {
  monitor: string;
  edge: Edge;
  span_start: number;
  span_end: number;
  offset: number;
  scale: number;
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
    /** 校准得到的 px/计数；0 表示未校准（引擎退回 xscale）。 */
    px_per_count: number;
  };
  keyboard: {
    forward: boolean;
    suppress_pc: boolean;
    layout: string;
    passthrough_keys: string[];
  };
  hotkeys: {
    toggle: string;
    panic: string;
    back: string;
    home: string;
    recents: string;
    notifications: string;
    power: string;
    volume_up: string;
    volume_down: string;
  };
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
    forward_audio: boolean;
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

export type DialogId = 'diagnostics' | 'wizard' | 'addDevice' | 'update' | null;

export interface ToastMessage {
  id: number;
  text: string;
  tone: 'ok' | 'warn' | 'err';
}
