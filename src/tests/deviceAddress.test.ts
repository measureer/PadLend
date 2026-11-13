import { describe, expect, it } from 'vitest';
import { normalizeDeviceAddress, validateDeviceAddress } from '../lib/deviceAddress';

describe('normalizeDeviceAddress', () => {
  it('补全 IPv4 和主机名的默认端口', () => {
    expect(normalizeDeviceAddress('192.168.1.23')).toBe('192.168.1.23:5555');
    expect(normalizeDeviceAddress('tablet.local')).toBe('tablet.local:5555');
  });

  it('保留显式端口', () => {
    expect(normalizeDeviceAddress('192.168.1.23:37123')).toBe('192.168.1.23:37123');
  });

  it('处理 IPv6 方括号与裸地址', () => {
    expect(normalizeDeviceAddress('[fd7a::1]:5555')).toBe('[fd7a::1]:5555');
    expect(normalizeDeviceAddress('[fd7a::1]')).toBe('[fd7a::1]:5555');
    expect(normalizeDeviceAddress('fd7a::1')).toBe('[fd7a::1]:5555');
  });

  it('拒绝残缺的方括号地址和空输入', () => {
    expect(normalizeDeviceAddress('[fd7a::1]:abc')).toBe('[fd7a::1]:abc');
    expect(normalizeDeviceAddress('[]:5555')).toBe('');
    expect(normalizeDeviceAddress('   ')).toBe('');
  });
});

describe('validateDeviceAddress', () => {
  it('接受 IPv4、主机名、IPv6 和跨网主机名', () => {
    expect(validateDeviceAddress('192.168.1.23')).toBe('');
    expect(validateDeviceAddress('tablet.local:5555')).toBe('');
    expect(validateDeviceAddress('[fd7a::1]:37123')).toBe('');
    expect(validateDeviceAddress('pad.example.com:5555')).toBe('');
  });

  it('拒绝空地址和非法主机名', () => {
    expect(validateDeviceAddress('')).toContain('请填写');
    expect(validateDeviceAddress('bad_host:5555')).toContain('主机名');
    expect(validateDeviceAddress('pad..local:5555')).toBe('');
  });

  it('拒绝非法 IPv6 与缺端口方括号', () => {
    expect(validateDeviceAddress('[gggg::1]:5555')).toContain('IPv6');
    expect(validateDeviceAddress('[fd7a::1]:abc')).toContain('端口');
  });

  it('校验端口范围和配对码', () => {
    expect(validateDeviceAddress('192.168.1.23:0')).toContain('端口');
    expect(validateDeviceAddress('192.168.1.23:70000')).toContain('端口');
    expect(validateDeviceAddress('192.168.1.23:5555', '12345')).toContain('配对码');
    expect(validateDeviceAddress('192.168.1.23:5555', '123456')).toBe('');
  });
});
