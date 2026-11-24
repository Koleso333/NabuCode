import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

export default defineConfig({
  plugins: [solid()],
  clearScreen: false,
  build: { outDir: "build/ui" },
  server: {
    port: 1431, strictPort: true, watch: { ignored: ["**/build/**"] },
    // Browser dev mode has no Tauri backend; fonts.google.com sends no CORS headers
    proxy: { "/google-fonts": { target: "https://fonts.google.com", changeOrigin: true, rewrite: path => path.replace(/^\/google-fonts/, "") } },
  },
});
