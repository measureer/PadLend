/** 设置页的声明式 schema：一份定义同时驱动渲染、取值、写回与校验。
 * 旧版 12 个控件各自赋值、各自校验、各自标红；现在只有这一处规则。 */

import type { Config } from '../api/types';

export type SettingSection = 'device' | 'motion' | 'keyboard' | 'hotkeys' | 'ui' | 'misc';

export interface FieldDef {
  id: string;
  section: SettingSection;
  key: string;
  label: string;
  hint: string;
  kind: 'toggle' | 'slider' | 'number' | 'select' | 'hotkey';
  min?: number;
  max?: number;
  step?: number;
  display?: (value: number) => number;
  persist?: (value: number) => number;
  format?: (value: number) => string;
  /** 滑条数值框里的文本与解析（缺省就是内部值的字符串形式）。 */
  text?: (value: number) => string;
  parse?: (text: string) => number | null;
  /** 数值框后缀单位，例如 × / px / Hz。 */
  unit?: string;
  options?: { value: string; label: string }[];
  placeholder?: string;
  /** 生效方式：live = 保存即生效；reconnect = 下次连接平板生效；restart = 重启 PadLend 生效。
   *  这是引擎真实行为，不是文案：引擎只在连接/启动时读这几项，界面上必须写清楚。 */
  applies?: 'live' | 'reconnect' | 'restart';
}

export interface SettingGroup {
  id: string;
  title: string;
  icon: string;
  description: string;
  /** 设置页的列：main = 左列（鼠标 / 键盘 / 快捷键），side = 右列（杂项）。 */
  column: 'main' | 'side';
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
    title: '鼠标',
    icon: 'pointer',
    description: '指针速度与进入 / 交还边缘的判定距离',
    column: 'main',
    fields: [
      {
        id: 'gain', section: 'motion', key: 'gain', label: '速度', kind: 'slider',
        min: 25, max: 300, step: 1,
        display: (v) => Math.round(v * 100), persist: (v) => Math.round(v) / 100,
        format: (v) => `${(v / 100).toFixed(2)}×`,
        text: (v) => (v / 100).toFixed(2),
        parse: (text) => {
          const parsed = Number.parseFloat(text.replace(',', '.'));
          return Number.isFinite(parsed) ? Math.round(parsed * 100) : null;
        },
        unit: '×',
        hint: '平板指针的移动倍率。数值越大，同样一段鼠标位移在平板上走得越远。',
      },
      {
        id: 'report_hz', section: 'motion', key: 'report_hz', label: '上报频率', kind: 'select',
        options: HZ_OPTIONS,
        hint: '每秒向平板发多少次指针位置。默认 125 Hz，无线连接也够跟手；想更跟手可以调高，抖动时调低。',
      },
      {
        id: 'arm_px', section: 'motion', key: 'arm_px', label: '触发阈值', kind: 'slider',
        min: 1, max: 200, step: 1, format: (v) => `${v} px`, unit: 'px',
        hint: '鼠标从屏幕边缘继续往外推多少像素才正式接管平板。太小容易误触。',
      },
      {
        id: 'exit_overflow_px', section: 'motion', key: 'exit_overflow_px', label: '穿回阈值', kind: 'slider',
        min: 4, max: 120, step: 1, format: (v) => `${v} px`, unit: 'px',
        hint: '在平板边缘反向拖回多少像素才交还 PC。调大可以避免边缘点按晃动误退。',
      },
      {
        id: 'px_per_count', section: 'motion', key: 'px_per_count', label: '指针比例', kind: 'number',
        min: 0, max: 10, step: 0.01,
        text: (v) => v.toFixed(2),
        parse: (text) => {
          const parsed = Number.parseFloat(text.replace(',', '.'));
          if (!Number.isFinite(parsed)) return null;
          // 2.0.2：合法域只有 0 或 1..10。之前这里原样返回，(0,1) 的值能进草稿
          // 却过不了校验，保存时才弹「配置未保存：motion.px_per_count 必须是
          // 0 或 1..10」—— 用户看不出到底哪儿不对。直接在入口夹住。
          if (parsed <= 0) return 0;
          if (parsed < 1) return 1;
          if (parsed > 10) return 10;
          return parsed;
        },
        unit: 'px/计数',
        applies: 'live',
        hint: '每个 HID 计数在平板上走多少像素，填 1–10；0 表示按分辨率推理论值（不写镜头距离）。自动校准会随手速浮动（推得越快测得越小）自动校准会随手速浮动（推得越快测得越小），填比校准值低 10–20% 更稳：估计器宁可偏晚交还，也不会在屏幕中间掉回 PC；觉得回 PC 太迟钝就往大调。',
      },
    ],
  },
  {
    id: 'keyboard',
    title: '键盘',
    icon: 'keyboard',
    description: '接管期间的按键转发与拦截',
    column: 'main',
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
    description: '系统级全局热键，窗口不在前台也有效；也可以快速连按两次同一个修饰键（如 Ctrl+Ctrl）',
    column: 'main',
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
    id: 'tablet_keys',
    title: '平板快捷键',
    icon: 'tablet',
    description: '不用接管也能操作平板，组合键可逐个改',
    column: 'main',
    fields: [
      {
        id: 'back', section: 'hotkeys', key: 'back', label: '后退', kind: 'hotkey',
        placeholder: 'Ctrl+Alt+B', hint: '给平板发一次「返回」键，等于平板上划手势的返回。',
      },
      {
        id: 'home', section: 'hotkeys', key: 'home', label: '返回主页', kind: 'hotkey',
        placeholder: 'Ctrl+Alt+H', hint: '回到平板桌面。',
      },
      {
        id: 'recents', section: 'hotkeys', key: 'recents', label: '最近任务', kind: 'hotkey',
        placeholder: 'Ctrl+Alt+R', hint: '打开平板的最近任务（多任务）界面。',
      },
      {
        id: 'notifications', section: 'hotkeys', key: 'notifications', label: '通知栏', kind: 'hotkey',
        placeholder: 'Ctrl+Alt+N', hint: '展开平板的通知面板；与下拉状态栏效果相同。',
      },
      {
        id: 'power', section: 'hotkeys', key: 'power', label: '电源键', kind: 'hotkey',
        placeholder: 'Ctrl+Alt+P', hint: '模拟平板的电源键（锁屏 / 亮屏）。',
      },
      {
        id: 'volume_up', section: 'hotkeys', key: 'volume_up', label: '音量 +', kind: 'hotkey',
        placeholder: 'Ctrl+Alt+PageUp', hint: '调高平板媒体音量。',
      },
      {
        id: 'volume_down', section: 'hotkeys', key: 'volume_down', label: '音量 -', kind: 'hotkey',
        placeholder: 'Ctrl+Alt+PageDown', hint: '调低平板媒体音量。',
      },
    ],
  },
  {
    id: 'misc',
    title: '杂项',
    icon: 'sliders',
    description: '启动、托盘、剪贴板与平板行为',
    column: 'side',
    fields: [
      {
        id: 'auto_connect', section: 'device', key: 'auto_connect', label: '打开软件自动连接上次连接的设备', kind: 'toggle',
        applies: 'restart',
        hint: '启动后自动连上次用过的那台设备（地址存在配置里）。关闭后停在设备页，需要手动点「连接此设备」。引擎只在启动时读这一项，保存后要重启 PadLend。',
      },
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
        applies: 'reconnect',
        hint: '连接期间把平板的「熄屏时间」临时调到 5 小时（原本更长就不动），断开或退出时还原；异常退出会在下次启动补还原。默认关闭。',
      },
      {
        id: 'forward_audio', section: 'misc', key: 'forward_audio', label: '转发平板声音', kind: 'toggle',
        applies: 'reconnect',
        hint: '把平板的声音实时转到电脑，平板自身静音。需要 Android 11+，首次开启可能要在平板上确认；音频通道在连接时建立，改完要断开再连一次。',
      },
    ],
  },
];

/** 配置里全部快捷键字段，顺序与 HotkeyAction 一致（改这里要同步引擎的 HotkeyCfg）。 */
export const HOTKEY_KEYS = [
  'toggle', 'panic', 'back', 'home', 'recents', 'notifications', 'power', 'volume_up', 'volume_down',
] as const;

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
  if (!(motion.px_per_count === 0 || (motion.px_per_count >= 1 && motion.px_per_count <= 10))) {
    // 2.0.2：带上当前值，否则用户只看到一句规则，不知道哪个数越界。
    problems.push(`motion.px_per_count 必须是 0 或 1..10（当前 ${motion.px_per_count}）`);
  }
  if (motion.edge_band_px < 1) problems.push('motion.edge_band_px 必须 ≥1');
  if (motion.debounce_ms < 0) problems.push('motion.debounce_ms 不能为负');
  if (motion.boundary_inset_px < 0) problems.push('motion.boundary_inset_px 不能为负');
  if (!keyboard.layout.trim()) problems.push('keyboard.layout 不能为空');
  if (!Array.isArray(keyboard.passthrough_keys)) problems.push('keyboard.passthrough_keys 必须是列表');
  for (const key of HOTKEY_KEYS) {
    if (!String(hotkeys[key] ?? '').trim()) problems.push(`hotkeys.${key} 不能为空`);
  }
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
