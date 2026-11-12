/** 统一线性图标集：内联 SVG，无字体/图片请求，随主题 currentColor 自动变色。 */

import { memo } from 'react';

export type IconName =
  | 'devices' | 'layout' | 'settings' | 'logs' | 'refresh' | 'add' | 'sun' | 'moon'
  | 'dots' | 'gear' | 'link' | 'unlink' | 'close' | 'wifi' | 'save' | 'fit'
  | 'pointer' | 'keyboard' | 'bolt' | 'sliders' | 'check' | 'alert' | 'info'
  | 'copy' | 'folder' | 'download' | 'shield' | 'tablet' | 'monitor' | 'trash'
  | 'minimize' | 'maximize' | 'power' | 'chevronLeft' | 'chevronRight' | 'play'
  | 'clipboard' | 'search' | 'sparkle';

const PATHS: Record<IconName, string> = {
  devices: 'M4 5h11v11H4zM4 20h11M17 9h3v9h-3zM8 8h3',
  layout: 'M3 5h13v10H3zM18 8h3v7h-3zM3 19h18M7 15v4',
  settings: 'M12 15.2a3.2 3.2 0 1 0 0-6.4 3.2 3.2 0 0 0 0 6.4zM19.4 15a1.7 1.7 0 0 0 .34 1.87l.06.06-1.9 1.9-.06-.06a1.7 1.7 0 0 0-2.9 1.2V21h-2.7v-.9a1.7 1.7 0 0 0-2.9-1.2l-.06.06-1.9-1.9.06-.06A1.7 1.7 0 0 0 7.2 15H6.3v-2.7h.9a1.7 1.7 0 0 0 1.2-2.9l-.06-.06 1.9-1.9.06.06a1.7 1.7 0 0 0 2.9-1.2V5.4h2.7v.9a1.7 1.7 0 0 0 2.9 1.2l.06-.06 1.9 1.9-.06.06A1.7 1.7 0 0 0 20.8 12h.9v2.7h-.9a1.3 1.3 0 0 0-1.4.3z',
  logs: 'M5 4h14v16H5zM8 9h8M8 13h8M8 17h5',
  refresh: 'M20 11a8 8 0 1 0-2.3 5.7M20 5v6h-6',
  add: 'M12 5v14M5 12h14',
  sun: 'M12 17a5 5 0 1 0 0-10 5 5 0 0 0 0 10zM12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4',
  moon: 'M20 14.5A8.5 8.5 0 0 1 9.5 4 8.5 8.5 0 1 0 20 14.5z',
  dots: 'M6 12h.01M12 12h.01M18 12h.01',
  gear: 'M12 15.5a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7zM4 12a8 8 0 0 1 .2-1.8l-1.7-1.3 2-3.4 2 .8a8 8 0 0 1 3-1.7L10 2h4l.5 2.6a8 8 0 0 1 3 1.7l2-.8 2 3.4-1.7 1.3a8 8 0 0 1 0 3.6l1.7 1.3-2 3.4-2-.8a8 8 0 0 1-3 1.7L14 22h-4l-.5-2.6a8 8 0 0 1-3-1.7l-2 .8-2-3.4 1.7-1.3A8 8 0 0 1 4 12z',
  link: 'M10 13a5 5 0 0 0 7.1 0l2.4-2.4a5 5 0 0 0-7.1-7.1L11 4.9M14 11a5 5 0 0 0-7.1 0l-2.4 2.4a5 5 0 0 0 7.1 7.1L13 19.1',
  unlink: 'M9 15l-2.5 2.5a4 4 0 0 1-5.7-5.7L3.4 9.2M15 9l2.5-2.5a4 4 0 0 1 5.7 5.7l-2.6 2.6M4 4l16 16',
  close: 'M6 6l12 12M18 6L6 18',
  wifi: 'M3 9a14 14 0 0 1 18 0M6.5 12.5a9 9 0 0 1 11 0M10 16a4 4 0 0 1 4 0M12 20h.01',
  save: 'M5 4h11l3 3v13H5zM8 4v6h8V4M8 20v-6h8v6',
  fit: 'M4 9V4h5M20 9V4h-5M4 15v5h5M20 15v5h-5',
  pointer: 'M5 3l14 9-6 1.5L10 20z',
  keyboard: 'M3 6h18v12H3zM7 10h.01M11 10h.01M15 10h.01M7 14h10',
  bolt: 'M13 2L5 14h6l-1 8 8-12h-6z',
  sliders: 'M4 7h16M4 17h16M9 4v6M15 14v6',
  check: 'M4 12l5 5L20 6',
  alert: 'M12 3l10 18H2zM12 9v5M12 18h.01',
  info: 'M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18zM12 11v5M12 8h.01',
  copy: 'M9 9h11v11H9zM5 15H4V4h11v1',
  folder: 'M3 6h6l2 2h10v11H3z',
  download: 'M12 3v12M7 11l5 5 5-5M4 20h16',
  shield: 'M12 3l8 3v6c0 5-3.5 8-8 9-4.5-1-8-4-8-9V6zM9 12l2 2 4-4',
  tablet: 'M5 3h14v18H5zM10 18h4',
  monitor: 'M3 5h18v12H3zM8 21h8M12 17v4',
  trash: 'M4 7h16M9 7V4h6v3M6 7l1 14h10l1-14M10 11v6M14 11v6',
  minimize: 'M5 12h14',
  maximize: 'M5 5h14v14H5z',
  power: 'M12 3v9M6.3 6.3a8 8 0 1 0 11.4 0',
  chevronLeft: 'M15 5l-7 7 7 7',
  chevronRight: 'M9 5l7 7-7 7',
  play: 'M8 5l11 7-11 7z',
  clipboard: 'M9 4h6v3H9zM6 6h12v15H6zM9 11h6M9 15h4',
  search: 'M11 19a8 8 0 1 0 0-16 8 8 0 0 0 0 16zM17 17l4 4',
  sparkle: 'M12 3l1.8 5.2L19 10l-5.2 1.8L12 17l-1.8-5.2L5 10l5.2-1.8zM19 17l.7 2 2 .7-2 .7-.7 2-.7-2-2-.7 2-.7z',
};

interface IconProps {
  name: IconName;
  size?: number;
  className?: string;
  strokeWidth?: number;
}

export const Icon = memo(function Icon({ name, size = 18, className, strokeWidth = 1.7 }: IconProps) {
  return (
    <svg
      className={className}
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={strokeWidth}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      <path d={PATHS[name]} />
    </svg>
  );
});
