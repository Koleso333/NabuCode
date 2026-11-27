import { defineConfig, type Plugin } from "vite";
import solid from "vite-plugin-solid";

// Chrome's extension pages refuse inline scripts; the shell's one is only for Tauri anyway
const dropInlineScripts: Plugin = {
  name: "drop-inline-scripts",
  transformIndexHtml: html => html.replace(/<script>[\s\S]*?<\/script>\s*/g, ""),
};

export default defineConfig(({ mode }) => mode === "panel" ? {
  // The side panel of the Chrome extension: assets are found next to its page
  plugins: [solid(), dropInlineScripts],
  base: "./",
  build: { outDir: "../extension/panel", emptyOutDir: true },
} : {
  plugins: [solid()],
  clearScreen: false,
  build: { outDir: "build/ui" },
  server: {
    port: 1431, strictPort: true, watch: { ignored: ["**/build/**"] },
    // Browser dev mode has no Tauri backend; fonts.google.com sends no CORS headers
    proxy: { "/google-fonts": { target: "https://fonts.google.com", changeOrigin: true, rewrite: path => path.replace(/^\/google-fonts/, "") } },
  },
});
