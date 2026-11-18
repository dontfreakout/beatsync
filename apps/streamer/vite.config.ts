import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";

// Tauri loads the dev server from a fixed port (tauri.conf.json `devUrl`) and prints the Rust
// build output in the same terminal, so Vite keeps the screen.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: { alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) } },
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: false,
    watch: { ignored: ["**/src-tauri/**", "**/crates/**", "**/target/**"] },
  },
});
