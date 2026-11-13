/** 单一状态源。按字段订阅（zustand selector），任何一次事件只重渲染相关组件。 */

import { create } from 'zustand';
import { defaultConfig } from '../api/mock';
import type {
  Bootstrap,
  Config,
  Desktop,
  DeviceFacts,
  DeviceItem,
  DialogId,
  LogEntry,
  Metrics,
  PageId,
  Placement,
  Theme,
  ToastMessage,
} from '../api/types';

const EMPTY_DESKTOP: Desktop = {
  monitors: [],
  segments: [],
  boundingBox: { left: 0, top: 0, right: 1920, bottom: 1080, width: 1920, height: 1080 },
};

export interface DiagnosticsState {
  text: string;
  status: string;
  progress: string;
  collecting: boolean;
}

interface AppStore {
  hydrated: boolean;
  version: string;
  config: Config;
  configIssues: string[];
  configPath: string;
  firstRun: boolean;
  simulated: boolean;
  theme: Theme;
  page: PageId;
  state: string;
  connected: boolean;
  facts: DeviceFacts | null;
  metrics: Metrics;
  desktop: Desktop;
  placement: Placement | null;
  layoutDirty: boolean;
  devices: DeviceItem[];
  selectedSerial: string;
  deviceError: string;
  scanHint: string;
  busy: Record<string, boolean>;
  logs: LogEntry[];
  logFilter: string;
  dialog: DialogId;
  menuOpen: boolean;
  toasts: ToastMessage[];
  configBanner: { tone: 'ok' | 'err' | 'warn'; text: string } | null;
  diagnostics: DiagnosticsState;
  paths: { configPath: string; logDir: string; noticesPath: string; version: string; license: string; appName: string };

  hydrate(data: Bootstrap): void;
  setPage(page: PageId): void;
  setTheme(theme: Theme): void;
  setConfig(config: Config, issues?: string[]): void;
  setState(state: string): void;
  setFacts(facts: DeviceFacts | null): void;
  setMetrics(metrics: Metrics): void;
  setDesktop(desktop: Desktop): void;
  setPlacement(placement: Placement | null, dirty?: boolean): void;
  setLayoutDirty(dirty: boolean): void;
  setDevices(devices: DeviceItem[], error?: string): void;
  setSelectedSerial(serial: string): void;
  setDeviceError(text: string): void;
  setScanHint(text: string): void;
  setBusy(key: string, value: boolean): void;
  appendLogs(entries: LogEntry[]): void;
  setLogFilter(value: string): void;
  openDialog(dialog: DialogId): void;
  setMenuOpen(value: boolean): void;
  pushToast(text: string, tone?: ToastMessage['tone']): void;
  dismissToast(id: number): void;
  setConfigBanner(value: AppStore['configBanner']): void;
  setDiagnostics(value: Partial<DiagnosticsState>): void;
  setPaths(paths: AppStore['paths']): void;
}

let toastId = 0;
export const MAX_LOGS = 2000;

export const useAppStore = create<AppStore>()((set) => ({
  hydrated: false,
  version: '1.2.1',
  config: defaultConfig(),
  configIssues: [],
  configPath: '',
  firstRun: false,
  simulated: false,
  theme: 'dark',
  page: 'devices',
  state: '',
  connected: false,
  facts: null,
  metrics: {},
  desktop: EMPTY_DESKTOP,
  placement: null,
  layoutDirty: false,
  devices: [],
  selectedSerial: '',
  deviceError: '',
  scanHint: '',
  busy: {},
  logs: [],
  logFilter: '',
  dialog: null,
  menuOpen: false,
  toasts: [],
  configBanner: null,
  diagnostics: { text: '', status: '尚未采集', progress: '', collecting: false },
  paths: { configPath: '', logDir: '', noticesPath: '', version: '1.2.1', license: 'Apache-2.0', appName: 'PadLend' },

  hydrate: (data) =>
    set({
      hydrated: true,
      version: data.version,
      config: data.config,
      configIssues: data.configIssues,
      configPath: data.configPath,
      firstRun: data.firstRun,
      simulated: data.simulated,
      theme: data.theme,
      state: data.state,
      connected: data.connected,
      facts: data.facts,
      metrics: data.metrics,
      desktop: data.desktop,
      placement: data.placement,
      layoutDirty: false,
      devices: data.devices,
      logs: data.logs,
    }),
  setPage: (page) => set({ page, menuOpen: false }),
  setTheme: (theme) => set({ theme }),
  setConfig: (config, issues) => set({ config, configIssues: issues ?? [] }),
  setState: (state) => set({ state }),
  setFacts: (facts) => set({ facts, connected: facts !== null }),
  setMetrics: (metrics) => set({ metrics }),
  setDesktop: (desktop) => set({ desktop }),
  setPlacement: (placement, dirty = true) => set({ placement, layoutDirty: placement ? dirty : false }),
  setLayoutDirty: (layoutDirty) => set({ layoutDirty }),
  setDevices: (devices, error = '') =>
    set((s) => ({
      devices,
      deviceError: error || s.deviceError,
      selectedSerial: devices.some((d) => d.serial === s.selectedSerial)
        ? s.selectedSerial
        : (devices.find((d) => d.online)?.serial ?? devices[0]?.serial ?? ''),
    })),
  setSelectedSerial: (selectedSerial) => set({ selectedSerial }),
  setDeviceError: (deviceError) => set({ deviceError }),
  setScanHint: (scanHint) => set({ scanHint }),
  setBusy: (key, value) => set((s) => ({ busy: { ...s.busy, [key]: value } })),
  appendLogs: (entries) =>
    set((s) => {
      if (!entries.length) return s;
      const next = [...s.logs, ...entries];
      if (next.length > MAX_LOGS) next.splice(0, next.length - MAX_LOGS);
      return { logs: next };
    }),
  setLogFilter: (logFilter) => set({ logFilter }),
  openDialog: (dialog) => set({ dialog, menuOpen: false }),
  setMenuOpen: (menuOpen) => set({ menuOpen }),
  pushToast: (text, tone = 'ok') =>
    set((s) => ({ toasts: [...s.toasts.slice(-3), { id: ++toastId, text, tone }] })),
  dismissToast: (id) => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),
  setConfigBanner: (configBanner) => set({ configBanner }),
  setDiagnostics: (value) => set((s) => ({ diagnostics: { ...s.diagnostics, ...value } })),
  setPaths: (paths) => set({ paths }),
}));
