/** 全局快捷键：键盘事件 → 配置文本，以及配置文本的本地校验。
 *
 * 规则必须与引擎 padlend-core/src/win/hotkeys.rs 的 parse_combo 完全一致，否则
 * 设置页显示「已保存」而引擎注册失败，用户只会看到快捷键没反应：
 * - 大小写不敏感，分隔符是 +；
 * - 修饰键 Ctrl / Alt / Shift / Win（Win 也接受 Meta / Super，本文件只产出 Win）；
 * - 主键 A-Z、0-9、F1-F24 或具名键（Space / PageUp / Up / Enter / Semicolon…）；
 * - 必须带 Ctrl / Alt / Win 之一 —— 只按 Shift 的组合会吞掉正常打字，引擎直接判无效。
 *
 * 引擎是最终权威：这里只保证「录制出来的文本引擎一定认得」，引擎回传的
 * 「快捷键「…」无效」日志仍以引擎为准。
 */

/** 修饰键的显示名（顺序即录制顺序）。 */
export const MODIFIERS = ['Ctrl', 'Alt', 'Shift', 'Win'] as const;

/** 只按下修饰键时的 code（此时继续等待主键，不要中断录制）。 */
export const MODIFIER_CODES = [
  'ControlLeft', 'ControlRight', 'AltLeft', 'AltRight',
  'ShiftLeft', 'ShiftRight', 'MetaLeft', 'MetaRight',
  'OSLeft', 'OSRight',
];

/** event.code → 引擎认得的按键名。 */
const CODE_NAMES: Record<string, string> = {
  Space: 'Space',
  Backspace: 'Backspace',
  Tab: 'Tab',
  Enter: 'Enter',
  NumpadEnter: 'Enter',
  Escape: 'Escape',
  PageUp: 'PageUp',
  PageDown: 'PageDown',
  Home: 'Home',
  End: 'End',
  ArrowUp: 'Up',
  ArrowDown: 'Down',
  ArrowLeft: 'Left',
  ArrowRight: 'Right',
  Insert: 'Insert',
  Delete: 'Delete',
  NumLock: 'NumLock',
  ScrollLock: 'ScrollLock',
  Semicolon: 'Semicolon',
  Equal: 'Equal',
  Comma: 'Comma',
  Minus: 'Minus',
  Period: 'Period',
  Slash: 'Slash',
  Backquote: 'Grave',
  BracketLeft: 'BracketLeft',
  BracketRight: 'BracketRight',
  Backslash: 'Backslash',
  Quote: 'Quote',
  NumpadAdd: 'Plus',
  NumpadSubtract: 'Minus',
};

/** 具名键（小写）—— 与引擎 vk_for_key 的具名分支一一对应。 */
const NAMED_KEYS = new Set([
  'backspace', 'tab', 'enter', 'return', 'pause', 'capslock', 'esc', 'escape', 'space',
  'pageup', 'pgup', 'pagedown', 'pgdn', 'end', 'home',
  'arrowleft', 'left', 'arrowup', 'up', 'arrowright', 'right', 'arrowdown', 'down',
  'printscreen', 'insert', 'ins', 'delete', 'del', 'numlock', 'scrolllock',
  'semicolon', 'plus', 'equal', 'comma', 'minus', 'period', 'slash', 'grave',
  'bracketleft', 'backslash', 'bracketright', 'quote',
]);

/** 标点（字符形式也可直接写进配置）。 */
const PUNCTUATION = '+=,<-_.>/?:;[]\\|\'"`~';

type KeyboardLike = {
  code?: string;
  key?: string;
  ctrlKey: boolean;
  altKey: boolean;
  shiftKey: boolean;
  metaKey: boolean;
};

/** 单个按键 → 引擎认得的按键名；不支持的键返回 null（如小键盘 * / 媒体键）。 */
export function keyNameFromCode(code: string | undefined, key = ''): string | null {
  if (code && CODE_NAMES[code]) return CODE_NAMES[code];
  if (code && /^Key[A-Z]$/.test(code)) return code.slice(3);
  if (code && /^Digit[0-9]$/.test(code)) return code.slice(5);
  if (code && /^F([1-9]|1[0-9]|2[0-4])$/.test(code)) return code;
  // 没有 code（老 WebView / 合成事件）时退回 event.key：字母、数字与 ASCII 标点都能直接写。
  if (key.length === 1) {
    const upper = key.toUpperCase();
    if (/^[A-Z0-9]$/.test(upper)) return upper;
    if (PUNCTUATION.includes(key)) return key;
  }
  return null;
}

/** 键盘事件 → 组合键文本。只按修饰键、或不支持的键返回 null。 */
export function comboFromEvent(event: KeyboardLike): string | null {
  const name = keyNameFromCode(event.code, event.key);
  if (!name) return null;
  const parts: string[] = [];
  if (event.ctrlKey) parts.push('Ctrl');
  if (event.altKey) parts.push('Alt');
  if (event.shiftKey) parts.push('Shift');
  if (event.metaKey) parts.push('Win');
  parts.push(name);
  return parts.join('+');
}

function modifierOf(token: string): (typeof MODIFIERS)[number] | null {
  switch (token.toLowerCase()) {
    case 'ctrl':
    case 'control':
      return 'Ctrl';
    case 'alt':
    case 'menu':
      return 'Alt';
    case 'shift':
      return 'Shift';
    case 'win':
    case 'meta':
    case 'super':
    case 'cmd':
    case 'command':
      return 'Win';
    default:
      return null;
  }
}

function keyKnown(name: string): boolean {
  const lower = name.toLowerCase();
  if (lower.length === 1) return /[a-z0-9]/.test(lower) || PUNCTUATION.includes(name);
  if (/^f([1-9]|1[0-9]|2[0-4])$/.test(lower)) return true;
  return NAMED_KEYS.has(lower);
}

/** 校验组合键文本：'' 表示合法，否则是给用户看的错误文案。 */
export function comboError(text: string): string {
  const raw = String(text ?? '').trim();
  if (!raw) return '组合键不能为空';
  const modifiers: string[] = [];
  let main = '';
  for (const part of raw.split('+')) {
    const token = part.trim();
    if (!token) {
      // "Ctrl++" 表示主键就是 +；引擎同样这样处理。
      main = '+';
      continue;
    }
    const modifier = modifierOf(token);
    if (modifier) {
      modifiers.push(modifier);
      continue;
    }
    if (main) return `组合键里只能有一个主键：${raw}`;
    main = token;
  }
  if (!main) return '缺少主键（比如 S 或 F5）';
  if (!modifiers.some((name) => name === 'Ctrl' || name === 'Alt' || name === 'Win')) {
    return '至少要有 Ctrl / Alt / Win 之一';
  }
  if (!keyKnown(main)) return `不支持的按键：${main}`;
  return '';
}

/** 组合键文本 → 界面上逐个键帽的数组（渲染小标签用）。 */
export function comboKeys(text: string): string[] {
  return String(text ?? '')
    .split('+')
    .map((part) => part.trim())
    .filter(Boolean);
}
