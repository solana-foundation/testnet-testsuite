import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// `pnpm --filter dashboard dev` serves the UI on :5173 against a running
// control (:8182); `just dashboard-build` writes dist/, which control serves.
export default defineConfig({
  plugins: [react()],
  server: {
    host: "127.0.0.1",
    proxy: {
      "/api/live": { target: "ws://127.0.0.1:8182", ws: true },
      "/api": "http://127.0.0.1:8182",
    },
  },
});
