import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'
import { fileURLToPath, URL } from 'node:url'

// https://vite.dev/config/
export default defineConfig({
  plugins: [
    vue(),
    // 移除 crossorigin 属性，Tauri 自定义协议需要
    {
      name: 'remove-crossorigin',
      transformIndexHtml(html) {
        return html.replace(/\s+crossorigin(?:="[^"]*")?/g, '')
      },
    },
  ],

  // 使用相对路径，确保 Tauri 独立 exe 能正确加载资源
  base: './',

  // 防止 Vite 把 Rust 保留字当端口
  clearScreen: false,

  server: {
    port: 5173,
    strictPort: true,
    // Tauri 在 Windows 上通过 127.0.0.1 连接
    host: '127.0.0.1',
  },

  // 环境变量传递给 Tauri
  envPrefix: ['VITE_', 'TAURI_'],

  build: {
    // 单入口：只有主窗口 index.html（控制窗口/手机控制功能已移除）
    rollupOptions: {
      input: {
        main: fileURLToPath(new URL('./index.html', import.meta.url)),
      },
    },
  },
})
