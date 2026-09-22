import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// WS target of the Rust simulator. Override with:
//   VITE_WS_URL=ws://host:port bun run dev
export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
  },
  build: {
    outDir: "dist",
    sourcemap: false,
    target: "es2022",
  },
});
