import { describe, expect, it } from 'vitest';
import { defaultConfig } from '../api/mock';
import {
  SETTING_GROUPS,
  readField,
  settingsPatch,
  validateConfig,
  writeField,
} from '../lib/configSchema';

const gainField = SETTING_GROUPS[0].fields[0];
const reportField = SETTING_GROUPS[0].fields[1];

describe('声明式设置 schema', () => {
  it('覆盖 23 个可编辑字段，且每个字段都有提示文案', () => {
    const fields = SETTING_GROUPS.flatMap((group) => group.fields);
    expect(fields).toHaveLength(23);
    expect(fields.every((field) => field.hint.length > 0)).toBe(true);
    expect(fields.map((f) => f.id)).toContain('close_to_tray');
    expect(fields.map((f) => f.id)).not.toContain('clipboard');

    // 左列放鼠标 / 键盘 / 快捷键 / 平板快捷键，右列只放杂项；标题按用户习惯叫「鼠标」。
    expect(SETTING_GROUPS.filter((g) => g.column === 'main').map((g) => g.title)).toEqual([
      '鼠标',
      '键盘',
      '快捷键',
      '平板快捷键',
    ]);
    expect(SETTING_GROUPS.filter((g) => g.column === 'side').map((g) => g.title)).toEqual(['杂项']);

    const keepScreen = fields.find((f) => f.id === 'keep_screen_on');
    expect(keepScreen?.hint).toContain('临时调到');
    expect(keepScreen?.hint).toContain('熄屏时间');
    expect(keepScreen?.hint).toContain('5 小时');
    expect(keepScreen?.hint).toContain('默认关闭');

    const audio = fields.find((f) => f.id === 'forward_audio');
    expect(audio?.label).toBe('转发平板声音');
    expect(audio?.hint).toContain('Android 11');
    expect(audio?.hint).toContain('断开再连一次');
    // 「改完要不要重连」必须写在设置项上，而不是靠用户猜。
    expect(audio?.applies).toBe('reconnect');
    expect(keepScreen?.applies).toBe('reconnect');

    // 自动连接开关放在杂项首项，写的是 device.auto_connect。
    const misc = SETTING_GROUPS.find((g) => g.id === 'misc');
    expect(misc?.fields[0].id).toBe('auto_connect');
    expect(misc?.fields[0].section).toBe('device');
    expect(misc?.fields[0].label).toBe('打开软件自动连接上次连接的设备');
    expect(misc?.fields[0].hint).toContain('重启');
    // 只有真的不是即时生效的项才挂徽章。
    expect(misc?.fields[0].applies).toBe('restart');

    // 安卓导航快捷键：7 个动作都能改，且都落在 hotkeys 段。
    const tablet = SETTING_GROUPS.find((g) => g.id === 'tablet_keys');
    expect(tablet?.fields.map((f) => f.id)).toEqual([
      'back', 'home', 'recents', 'notifications', 'power', 'volume_up', 'volume_down',
    ]);
    expect(tablet?.fields.every((f) => f.section === 'hotkeys' && f.kind === 'hotkey')).toBe(true);
    expect(tablet?.fields[0].placeholder).toBe('Ctrl+Alt+B');
  });

  it('自动连接写回 device 段且不影响同段其它字段', () => {
    const config = defaultConfig();
    config.device.serial = '192.168.68.129:35071';
    const field = SETTING_GROUPS.find((g) => g.id === 'misc')!.fields[0];
    expect(readField(config, field)).toBe(true);
    const next = writeField(config, field, false);
    expect(next.device.auto_connect).toBe(false);
    expect(next.device.serial).toBe('192.168.68.129:35071');
    expect(next.device.known).toEqual(config.device.known);
    expect(config.device.auto_connect).toBe(true);
    expect(settingsPatch(next).device?.auto_connect).toBe(false);
  });

  it('速度滑条以 1/100 倍率显示并写回', () => {
    const config = defaultConfig();
    expect(readField(config, gainField)).toBe(100);
    const next = writeField(config, gainField, 155);
    expect(next.motion.gain).toBeCloseTo(1.55);
    expect(config.motion.gain).toBe(1.0); // 不可变更新
  });

  it('上报频率写回为数字，且只接受固定档位', () => {
    const config = defaultConfig();
    expect(readField(config, reportField)).toBe('125');
    expect(writeField(config, reportField, '250').motion.report_hz).toBe(250);
    const invalid = { ...config, motion: { ...config.motion, report_hz: 300 } };
    expect(validateConfig(invalid)).toContain('motion.report_hz 必须是 125/250/500/1000 之一');
  });


  it('指针比例是可直接输入的数字框，写回 motion 段', () => {
    const config = defaultConfig();
    const field = SETTING_GROUPS[0].fields.find((f) => f.id === 'px_per_count')!;
    expect(field.kind).toBe('number');
    expect(field.unit).toBe('px/计数');
    expect(field.applies).toBe('live');
    expect(readField(config, field)).toBe(2);
    expect(writeField(config, field, 2.4).motion.px_per_count).toBe(2.4);
    const tooSmall = { ...config, motion: { ...config.motion, px_per_count: 0.5 } };
    expect(validateConfig(tooSmall)).toContain('motion.px_per_count 必须是 0 或 1..10');
  });

  it('校验规则与 config.py 一致并回报具体字段', () => {
    const config = defaultConfig();
    config.motion.gain = 5;
    config.hotkeys.toggle = '';
    config.device.server_port = 80;
    const problems = validateConfig(config);
    expect(problems).toContain('motion.gain 必须在 0.25..3.0');
    expect(problems).toContain('hotkeys.toggle 不能为空');
    expect(problems).toContain('device.server_port 必须在 1024..65535');
  });

  it('设置页补丁不会覆盖按设备方向保存的 layouts', () => {
    const patch = settingsPatch(defaultConfig());
    expect(patch).not.toHaveProperty('layouts');
    expect(patch).not.toHaveProperty('version');
    expect(patch.motion).toBeDefined();
  });
});
