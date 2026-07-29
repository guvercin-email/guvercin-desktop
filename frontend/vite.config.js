import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  build: {
    // The two chunks still over rollup's 500 kB default are the app core (~570 kB)
    // and the PDF export bundle (~640 kB, fetched only when the user exports).
    // Both are served off local disk by the Tauri shell, so the default threshold
    // flags them without anything being wrong. Raised rather than silenced, so a
    // chunk that genuinely balloons past this still gets reported.
    chunkSizeWarningLimit: 700,
    rollupOptions: {
      output: {
        // Without this everything lands in one ~2.9 MB chunk. The editor and the
        // pdf/screenshot pair are big, independent, and only needed once the user
        // reaches the screens that use them.
        manualChunks: {
          editor: ['prosemirror-commands', 'prosemirror-gapcursor', 'prosemirror-history',
            'prosemirror-inputrules', 'prosemirror-keymap', 'prosemirror-model',
            'prosemirror-schema-basic', 'prosemirror-schema-list', 'prosemirror-state',
            'prosemirror-transform', 'prosemirror-view'],
          // Only reached from the "export as PDF" path, which imports them
          // dynamically — keep them off the startup path.
          documents: ['pdf-lib', 'html2canvas'],
          i18n: ['i18next', 'i18next-http-backend', 'react-i18next'],
          react: ['react', 'react-dom', 'react-router-dom'],
        },
      },
    },
  },
  server: {
    host: '127.0.0.1',
    port: 5173,
    proxy: {
      '/api': {
        target: 'http://127.0.0.1:5000',
        changeOrigin: true,
      },
    },
  },
})
