import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// Tauri 需要一个固定端口，且关闭清空屏幕/Vite 的覆盖层以便调试原生窗口。
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: '127.0.0.1',
    watch: { ignored: ['**/src-tauri/**', '**/service/**', '**/dist/**'] },
  },
  build: {
    target: 'es2022',
    // 与 Python/PyInstaller 的发布目录 dist/ 分开，避免前端构建清空既有发布产物。
    outDir: 'frontend-dist',
    // 浏览器预览（无 Tauri 运行时）走 mock 后端，产物可用静态服务器直接打开。
    base: './',
    sourcemap: false,
    cssCodeSplit: false,
    chunkSizeWarningLimit: 900,
    rollupOptions: {
      output: {
        manualChunks: {
          react: ['react', 'react-dom', 'zustand'],
        },
      },
    },
  },
  test: {
    environment: 'jsdom',
    globals: true,
    include: ['src/tests/**/*.test.{ts,tsx}'],
  },
} as Parameters<typeof defineConfig>[0]);
