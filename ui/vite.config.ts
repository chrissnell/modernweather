import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

export default defineConfig({
  plugins: [sveltekit()],
  server: {
    port: 5173,
    // The dev loop of §6.4: `vite dev` proxies the management API to a running daemon,
    // so the daemon never has to rebuild to iterate on the UI.
    proxy: {
      '/api': {
        target: process.env.MW_API ?? 'http://127.0.0.1:8081',
        changeOrigin: false
      }
    }
  },
  build: {
    // The budget gate measures what the browser fetches, so no source maps ship.
    sourcemap: false,
    // Vite's default inlines assets under 4 KB as data URIs, which would move bytes out
    // of the chunk the budget attributes them to and into another one.
    assetsInlineLimit: 0
  }
});
