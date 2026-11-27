import { invoke } from '@tauri-apps/api/core'

const _fallback = import.meta.env?.VITE_API_URL || 'http://127.0.0.1:5500';

// Start with the fallback; replaced as soon as the real port arrives.
let _resolvedBase = _fallback;
let apiToken = import.meta.env?.DEV ? import.meta.env.VITE_API_TOKEN || '' : '';
let _readyResolve;
export const apiReady = new Promise(res => { _readyResolve = res; });

(async () => {
  try {
    for (let i = 0; i < 50; i++) {
      const connection = await invoke('get_backend_connection');
      if (connection) {
        const [port, token] = connection;
        apiToken = token;
        _resolvedBase = `http://127.0.0.1:${port}`;
        break;
      }
      await new Promise(r => setTimeout(r, 100));
    }
  } catch {
    // Not inside Tauri — keep the fallback.
  }
  _readyResolve();
})();

// Synchronous — safe to call from non-async contexts.
export const apiUrl = (path) => {
  if (path.startsWith('http')) return path;
  const url = new URL(path, _resolvedBase);
  if (apiToken && url.origin === new URL(_resolvedBase).origin
    && (url.pathname.startsWith('/api/') || url.pathname === '/health')) {
    url.searchParams.set('_guv_token', apiToken);
  }
  return url.href;
};
