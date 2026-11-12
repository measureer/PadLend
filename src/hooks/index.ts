/** 通用 hooks：节流/防抖、尺寸观察、全局快捷键、rAF 合帧。 */

import { useCallback, useEffect, useRef, useState, type RefObject } from 'react';
import { panic, toggleManual } from '../lib/actions';
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

function combo(event: KeyboardEvent): string {
  const parts: string[] = [];
  if (event.ctrlKey) parts.push('Ctrl');
  if (event.altKey) parts.push('Alt');
  if (event.shiftKey) parts.push('Shift');
  parts.push(event.key.length === 1 ? event.key.toUpperCase() : event.key);
  return parts.join('+');
}

export function useHotkeys(): void {
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const value = combo(event);
      if (value === 'Ctrl+,' || value === 'Ctrl+<') {
        event.preventDefault();
        useAppStore.getState().setPage('settings');
      } else if (value === 'Ctrl+Alt+S') {
        event.preventDefault();
        void toggleManual();
      } else if (value === 'Ctrl+Alt+Q') {
        event.preventDefault();
        void panic();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);
}
