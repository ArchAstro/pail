import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

const api = `http://127.0.0.1:${process.env.PAIL_WEB_PORT ?? 4747}`;

export default defineConfig({
  plugins: [react()],
  build: { outDir: "dist/public", sourcemap: false, chunkSizeWarningLimit: 600 },
  server: { port: 5173, proxy: { "/api": api } },
  test: {
    environment: "node",
    environmentMatchGlobs: [["src/**", "jsdom"], ["test/**/*.test.tsx", "jsdom"]],
    setupFiles: ["./test/setup.ts"],
    globalSetup: ["./test/global-setup.ts"],
    testTimeout: 30000,
    hookTimeout: 120000,
  },
});
