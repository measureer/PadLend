import { beforeEach, describe, expect, it, vi } from 'vitest';

const mocks = vi.hoisted(() => ({
  shutdown: vi.fn().mockResolvedValue(undefined),
  quitApp: vi.fn().mockResolvedValue(undefined),
  windowClose: vi.fn().mockResolvedValue(undefined),
}));

vi.mock('../api/client', () => ({
  backend: {
    shutdown: mocks.shutdown,
    quitApp: mocks.quitApp,
    windowClose: mocks.windowClose,
  },
  isTauri: true,
  RPC: {},
}));

import { quitApp } from '../lib/actions';

describe('退出动作', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('走 quit_app，不使用会被 close_to_tray 拦下的 windowClose', async () => {
    await quitApp();

    expect(mocks.shutdown).toHaveBeenCalledTimes(1);
    expect(mocks.quitApp).toHaveBeenCalledTimes(1);
    expect(mocks.windowClose).not.toHaveBeenCalled();
  });
});
