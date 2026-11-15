/** 通用 hooks：节流/防抖、尺寸观察、全局快捷键、rAF 合帧。 */

import { useCallback, useEffect, useRef, useState, type RefObject } from 'react';
import { useAppStore } from '../store/appStore';

export function useDebouncedValue<T>(value: T, delay = 120): T {
  const [debounced, setDebounced] = useState(value);
  useEffect(() => {
    const timer = window.setTimeout(() => setDebounced(value), delay);
    return () => window.clearTimeout(timer);
  }, [value, delay]);
  return debounced;
}

export function useElementSize(ref: RefObject<HTMLElement | null>): { width: number; height: number } {
  const [size, setSize] = useState({ width: 0, height: 0 });
  useEffect(() => {
    const element = ref.current;
    if (!element) return;
    let frame = 0;
    const observer = new ResizeObserver((entries) => {
      const rect = entries[0]?.contentRect;
      if (!rect) return;
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => setSize({ width: rect.width, height: rect.height }));
    });
    observer.observe(element);
    return () => {
      cancelAnimationFrame(frame);
      observer.disconnect();
    };
  }, [ref]);
  return size;
}

/** 高频回调合并到每帧一次，拖动/缩放时不产生 React 重渲染风暴。 */
export function useRafCallback<A extends unknown[]>(callback: (...args: A) => void): (...args: A) => void {
  const callbackRef = useRef(callback);
  const frameRef = useRef(0);
  const argsRef = useRef<A | null>(null);
  callbackRef.current = callback;
  useEffect(() => () => cancelAnimationFrame(frameRef.current), []);
  return useCallback((...args: A) => {
    argsRef.current = args;
    if (frameRef.current) return;
    frameRef.current = requestAnimationFrame(() => {
      frameRef.current = 0;
      if (argsRef.current) callbackRef.current(...argsRef.current);
    });
  }, []);
}

/** 界面内快捷键：只保留 Ctrl+, 这种「打开设置」的页面级热键。
 *
 * 接管 / 交还、急停与全部平板快捷键都不在这里处理：它们由引擎 RegisterHotKey 注册，
 * 天生全局（窗口收进托盘也有效）。两边都监听会导致前台时双触发（切换两次 = 没切换），
 * 这正是 1.7.7 之前「改了快捷键没用」的另一半原因。 */
export function useHotkeys(): void {
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.ctrlKey && !event.altKey && !event.metaKey && event.key === ',') {
        event.preventDefault();
        useAppStore.getState().setPage('settings');
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);
}
