import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// Tauri-tailored Vite options (same shape as the official template):
// no clearScreen so Rust errors stay visible, and a fixed port because
// tauri.conf.json's devUrl expects one. Multi-page build: the main
// dashboard (index.html) and the quick search window (quick.html, the
// "quick" webview in tauri.conf.json) build as separate entries.
export default defineConfig({
  plugins: [svelte()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: "127.0.0.1",
  },
  watch: {
    // Never watch the Rust side or the build output.
    ignored: ["**/target/**", "**/src/**.rs"],
  },
  build: {
    rollupOptions: {
      input: {
        main: fileURLToPath(new URL("./index.html", import.meta.url)),
        quick: fileURLToPath(new URL("./quick.html", import.meta.url)),
      },
    },
  },
});
