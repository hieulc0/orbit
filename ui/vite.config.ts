import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

const api = process.env.ORBIT_UI_API || 'http://127.0.0.1:7700';
export default defineConfig({
  base: '/console/', plugins: [react()],
  server: { proxy: Object.fromEntries(['/runs', '/workers', '/queues', '/limits', '/definitions', '/identity', '/audit', '/packages', '/projects', '/protocol'].map(path => [path, api])) },
  build: { sourcemap: false },
});
