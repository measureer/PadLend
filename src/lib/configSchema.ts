/** 设置页的声明式 schema：一份定义同时驱动渲染、取值、写回与校验。
 * 旧版 12 个控件各自赋值、各自校验、各自标红；现在只有这一处规则。 */

import type { Config } from '../api/types';

export type SettingSection = 'motion' | 'keyboard' | 'hotkeys' | 'ui' | 'misc';

export interface FieldDef {
  id: string;
  section: SettingSection;
  key: string;
  label: string;
  hint: string;
  kind: 'toggle' | 'slider' | 'select' | 'hotkey';
  min?: number;
  max?: number;
  step?: number;
  display?: (value: number) => number;
  persist?: (value: number) => number;
  format?: (value: number) => string;
  options?: { value: string; label: string }[];
  placeholder?: string;
}

export interface SettingGroup {
  id: string;
  title: string;
  icon: string;
  description: string;
  fields: FieldDef[];
}

const HZ_OPTIONS = [125, 250, 500, 1000].map((value) => ({ value: String(value), label: `${value} Hz` }));
const LAYOUT_OPTIONS = [
  { value: 'us', label: 'US' },
  { value: 'uk', label: 'UK' },
  { value: 'de', label: 'DE' },
  { value: 'fr', label: 'FR' },
  { value: 'es', label: 'ES' },
  { value: 'ru', label: 'RU' },
  { value: 'ja', label: 'JA' },
];

export const SETTING_GROUPS: SettingGroup[] = [
  {
    id: 'motion',
    title: '手感',
    icon: 'pointer',
    description: '指针速度与进入 / 交还边缘的判定距离',
    fields: [
      {
        id: 'gain', section: 'motion', key: 'gain', label: '速度', kind: 'slider',
        min: 25, max: 300, step: 1,
        display: (v) => Math.round(v * 100), persist: (v) => Math.round(v) / 100,
        format: (v) => `${(v / 100).toFixed(2)}×`,
        hint: '平板指针的移动倍率。数值越大，同样一段鼠标位移在平板上走得越远。',
      },
      {
        id: 'report_hz', section: 'motion', key: 'report_hz', label: '上报频率', kind: 'select',
        options: HZ_OPTIONS,
        hint: '每秒向平板发多少次指针位置。无线连接抖动时可以降到 500 或 250 Hz。',
      },
      {
        id: 'arm_px', section: 'motion', key: 'arm_px', label: '触发阈值', kind: 'slider',
        min: 1, max: 200, step: 1, format: (v) => `${v} px`,
        hint: '鼠标从屏幕边缘继续往外推多少像素才正式接管平板。太小容易误触。',
      },
      {
        id: 'exit_overflow_px', section: 'motion', key: 'exit_overflow_px', label: '穿回阈值', kind: 'slider',
        min: 4, max: 120, step: 1, format: (v) => `${v} px`,
        hint: '在平板边缘反向拖回多少像素才交还 PC。调大可以避免边缘点按晃动误退。',
      },
    ],
  },
  {
    id: 'keyboard',
    title: '键盘',
    icon: 'keyboard',
    description: '接管期间的按键转发与拦截',
    fields: [
      {
        id: 'forward', section: 'keyboard', key: 'forward', label: '转发按键到平板', kind: 'toggle',
        hint: '接管期间按下的键盘按键会发到平板，支持中文输入法。',
      },
      {
        id: 'suppress_pc', section: 'keyboard', key: 'suppress_pc', label: '屏蔽本机按键', kind: 'toggle',
        hint: '接管期间把键盘输入从 Windows 拦下来，避免 Ctrl+W 同时关掉两边。',
      },
      {
        id: 'layout', section: 'keyboard', key: 'layout', label: '布局', kind: 'select',
        options: LAYOUT_OPTIONS,
        hint: '平板上当前输入法的键盘布局，影响按键到 HID 码的映射。',
      },
    ],
  },
  {
    id: 'hotkeys',
    title: '快捷键',
    icon: 'bolt',
    description: '全局生效的保命组合键',
    fields: [
      {
        id: 'toggle', section: 'hotkeys', key: 'toggle', label: '接管 / 交还', kind: 'hotkey',
        placeholder: 'Ctrl+Alt+S', hint: '手动接管平板，或把控制权交还给 PC。',
      },
      {
        id: 'panic', section: 'hotkeys', key: 'panic', label: '急停', kind: 'hotkey',
        placeholder: 'Ctrl+Alt+Q', hint: '松开所有按住的键、释放遮罩，把光标立刻还给 PC。',
      },
    ],
  },
  {
    id: 'misc',
    title: '杂项',
    icon: 'sliders',
    description: '托盘、剪贴板与平板行为',
    fields: [
      {
        id: 'tray_notifications', section: 'ui', key: 'tray_notifications', label: '托盘气泡通知', kind: 'toggle',
        hint: '连接成功、断开或出故障时，在右下角托盘弹一个气泡提醒。',
      },
      {
        id: 'close_to_tray', section: 'ui', key: 'close_to_tray', label: '关闭窗口时最小化到托盘', kind: 'toggle',
        hint: '点窗口 X 时只收进托盘继续运行；不勾选则直接退出。',
      },
      {
        id: 'clipboard_sync', section: 'misc', key: 'clipboard_sync', label: '剪贴板同步', kind: 'toggle',
        hint: '双向同步文本：PC 复制自动写入平板，平板复制也会写回 Windows。只写剪贴板，不敲键盘。',
      },
      {
        id: 'keep_screen_on', section: 'misc', key: 'keep_screen_on', label: '保持平板亮屏', kind: 'toggle',
        hint: '开启后会在连接期间临时把平板的系统「熄屏时间」调到 30 分钟（原本更长则保持不变），断开或退出时还原；PadLend 异常退出时下次启动会补还原。不需要让平板常亮时可以关闭。不会修改开发者选项里的「保持唤醒状态」。',
      },
    ],
  },
];

type SectionValue = Record<string, string | number | boolean | string[]>;

export function readField(config: Config, field: FieldDef): string | number | boolean {
  const raw = (config[field.section] as unknown as SectionValue)[field.key];
  if (field.kind === 'slider' && typeof raw === 'number' && field.display) return field.display(raw);
  if (field.kind === 'select' && field.id === 'report_hz') return String(raw);
  return raw as string | number | boolean;
}

export function writeField(config: Config, field: FieldDef, value: string | number | boolean): Config {
  let next: string | number | boolean = value;
  if (field.kind === 'slider' && typeof value === 'number' && field.persist) next = field.persist(value);
  if (field.id === 'report_hz') next = Number(value);
  return {
    ...config,
    [field.section]: {
      ...(config[field.section] as unknown as SectionValue),
      [field.key]: next,
    },
  } as Config;
}

/** 与 config.py::Config.validate() 同规则；服务端仍是最终权威。 */
export function validateConfig(config: Config): string[] {
  const problems: string[] = [];
  const { device, placement, motion, keyboard, hotkeys } = config;
  if (!(device.server_port >= 1024 && device.server_port <= 65535)) {
    problems.push('device.server_port 必须在 1024..65535');
  }
  if (!['left', 'right', 'top', 'bottom'].includes(placement.edge)) {
    problems.push('placement.edge 必须是 left/right/top/bottom');
  }
  if (!['proportional', 'center', 'one_to_one'].includes(placement.align)) {
    problems.push('placement.align 必须是 proportional/center/one_to_one');
  }
  if (placement.offset < 0 || placement.offset > 1) problems.push('placement.offset 必须在 0.0..1.0');
  if (placement.span_end <= placement.span_start) problems.push('placement.span_end 必须大于 span_start');
  if (!(motion.hid_clamp >= 1 && motion.hid_clamp <= 127)) problems.push('motion.hid_clamp 必须在 1..127');
  if (![125, 250, 500, 1000].includes(Number(motion.report_hz))) {
    problems.push('motion.report_hz 必须是 125/250/500/1000 之一');
  }
  if (!(motion.gain >= 0.25 && motion.gain <= 3.0)) problems.push('motion.gain 必须在 0.25..3.0');
  if (motion.arm_px < 1) problems.push('motion.arm_px 必须 ≥1');
  if (motion.arm_window_ms < 1) problems.push('motion.arm_window_ms 必须 ≥1');
  if (motion.exit_overflow_px < 1) problems.push('motion.exit_overflow_px 必须 ≥1');
  if (motion.edge_band_px < 1) problems.push('motion.edge_band_px 必须 ≥1');
  if (motion.debounce_ms < 0) problems.push('motion.debounce_ms 不能为负');
  if (motion.boundary_inset_px < 0) problems.push('motion.boundary_inset_px 不能为负');
  if (!keyboard.layout.trim()) problems.push('keyboard.layout 不能为空');
  if (!Array.isArray(keyboard.passthrough_keys)) problems.push('keyboard.passthrough_keys 必须是列表');
  if (!hotkeys.toggle.trim()) problems.push('hotkeys.toggle 不能为空');
  if (!hotkeys.panic.trim()) problems.push('hotkeys.panic 不能为空');
  return problems;
}

/** 设置页只提交它负责的段，绝不覆盖按设备方向保存的 layouts。 */
export function settingsPatch(config: Config): Partial<Config> {
  return {
    device: config.device,
    placement: config.placement,
    motion: config.motion,
    keyboard: config.keyboard,
    hotkeys: config.hotkeys,
    ui: config.ui,
    misc: config.misc,
  };
}
