import { defineConfig } from 'vite';
import { dirname , resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import preact from '@preact/preset-vite';

// https://vitejs.dev/config/
export default defineConfig({
    plugins: [preact()],
    build: {
        rollupOptions: {
            input: {
                main: resolve(__dirname , 'index.html'),
                privacy: resolve(__dirname , 'privacy.html'),
            },
        },
    },
});
