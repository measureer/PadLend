/** 指针校准：3 个 RPC 的薄封装（引擎侧实现见 padlend-core/src/logic/router.rs）。 */

import { backend, RPC } from '../api/client';

export interface CalibrationReading {
  /** 是否仍在累计计数；用户交还 PC 后变 false。 */
  active: boolean;
  /** 当前是否处于接管状态。 */
  captured: boolean;
  countsX: number;
  countsY: number;
  width: number;
  height: number;
  /** 由「从左上角推到最底部」反推的 1 计数像素数；0 表示样本不足。 */
  pxPerCount: number;
}

/** 把平板指针钉到左上角并开始计数（必要时代为接管）。 */
export function calibrateBegin(): Promise<CalibrationReading> {
  return backend.call<CalibrationReading>(RPC.pointerCalibrateBegin);
}

export function calibrateState(): Promise<CalibrationReading> {
  return backend.call<CalibrationReading>(RPC.pointerCalibrateState);
}

/** 结束计数窗口（结果保留，可继续读取）。 */
export function calibrateEnd(): Promise<CalibrationReading> {
  return backend.call<CalibrationReading>(RPC.pointerCalibrateEnd);
}
