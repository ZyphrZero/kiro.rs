import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import path from 'path'

export default defineConfig({
  plugins: [react()],
  base: '/admin/',
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
    },
  },
  server: {
    // 监听所有网卡，方便从局域网 / 其它设备访问同一个 dev server
    host: '0.0.0.0',
    port: 5173,
    proxy: {
      '/api': {
        // 默认本地后端。要打线上：
        //   KIRO_API_TARGET=https://kiro.linkof.link pnpm run dev
        // 线上域名在 Cloudflare 后面，TLS 握手缺 SNI 会被直接断连
        // （"Client network socket disconnected before secure TLS connection"），
        // http-proxy 不会自动从 target 推导 servername，所以显式给出。
        target: process.env.KIRO_API_TARGET || 'http://localhost:8080',
        changeOrigin: true,
        secure: true,
        servername: process.env.KIRO_API_TARGET
          ? new URL(process.env.KIRO_API_TARGET).hostname
          : undefined,
      },
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    chunkSizeWarningLimit: 600,
    rolldownOptions: {
      output: {
        codeSplitting: {
          groups: [
            { name: 'react', test: /node_modules[\\/](react|react-dom|scheduler)[\\/]/ },
            { name: 'recharts', test: /node_modules[\\/](recharts|d3-[^\\/]+|victory-vendor|internmap|robust-predicates)[\\/]/ },
            { name: 'radix', test: /node_modules[\\/]@radix-ui[\\/]/ },
            { name: 'query', test: /node_modules[\\/]@tanstack[\\/]/ },
            { name: 'icons', test: /node_modules[\\/]lucide-react[\\/]/ },
            { name: 'sonner', test: /node_modules[\\/]sonner[\\/]/ },
            { name: 'vendor', test: /node_modules[\\/]/ },
          ],
        },
      },
    },
  },
})
