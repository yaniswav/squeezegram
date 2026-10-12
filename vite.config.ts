import { fileURLToPath, URL } from "node:url"
import tailwindcss from "@tailwindcss/vite"
import react from "@vitejs/plugin-react"
import { defineConfig } from "vite"

// Fixed dev port: src-tauri/tauri.conf.json (build.devUrl) points at it.
const DEV_PORT = 1420

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) },
  },
  // Keep Rust compiler output readable in the terminal when launched by `tauri dev`.
  clearScreen: false,
  server: {
    port: DEV_PORT,
    strictPort: true,
    // Rust rebuilds are handled by the Tauri CLI, not by Vite.
    watch: { ignored: ["**/src-tauri/**"] },
  },
  build: {
    // Assets are loaded from disk by the desktop shell, so bundle size is not a network concern.
    chunkSizeWarningLimit: 800,
  },
})
