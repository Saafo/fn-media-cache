import vue from '@vitejs/plugin-vue'
import { defineConfig } from 'vite'

export default defineConfig({
  plugins: [vue()],
  base: './',
  clearScreen: false,
  server: {
    host: '127.0.0.1',
    port: 5173,
    strictPort: true,
    watch: { ignored: ['**/src-tauri/**'] },
    proxy: {
      '/api': 'http://127.0.0.1:28006',
    },
  },
  build: {
    target: ['chrome105', 'safari14'],
    cssCodeSplit: false,
    license: { fileName: 'licenses.txt' },
    rolldownOptions: {
      output: {
        entryFileNames: 'assets/app.js',
        postBanner: '/*! Third-party notices: ../licenses.txt */',
        assetFileNames: (asset) => asset.names.some(name => name.endsWith('.css'))
          ? 'assets/app.css'
          : 'assets/[name][extname]',
      },
    },
  },
})
