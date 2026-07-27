import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri expects a fixed, predictable port — see src-tauri/tauri.conf.json's
// "devUrl". Vite's default (5173) doesn't match, so we pin it here.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
});
