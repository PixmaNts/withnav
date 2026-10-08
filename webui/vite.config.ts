/// <reference types="vitest/config" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { fallbacks } from "./dev/fallbacks.ts";
import { devEnv } from "./dev/legacyConfig.ts";

// The session server the dev build talks to. Defaults to a scratch server:
//   XDG_DATA_HOME=/tmp/opencode/withnav-webui-data navai serve --bind 127.0.0.1:7311
const target = devEnv("SERVER") ?? "http://127.0.0.1:7311";
// Workspaces listed by the dev stand-in for `GET /v1/workspaces`, colon-separated.
const workspaces = (devEnv("WORKSPACES") ?? "/tmp/opencode/withnav-webui-ws").split(":").filter(Boolean);

export default defineConfig({
  base: "/",
  plugins: [
    react(),
    {
      name: "withnav-dev-fallbacks",
      apply: "serve",
      configureServer(server) {
        // Dev only: a `nav web` server wants its bearer token, which the page served by Vite
        // does not have. Added here so both the proxy and the stand-ins send it.
        const token = devEnv("TOKEN");
        if (token)
          server.middlewares.use((req, _res, next) => {
            if (req.url?.startsWith("/v1") && !req.headers.authorization) req.headers.authorization = `Bearer ${token}`;
            next();
          });
        server.middlewares.use(fallbacks(target, workspaces));
      },
    },
  ],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    assetsDir: "assets",
    sourcemap: false,
    // One chunk is fine for a local app served from loopback; markdown + React dominate it.
    chunkSizeWarningLimit: 700,
  },
  server: {
    port: 5173,
    strictPort: true,
    proxy: {
      // SSE streams through untouched: http-proxy pipes the body as it comes.
      "/v1": { target, changeOrigin: false, ws: false },
    },
  },
  test: {
    environment: "node",
    include: ["src/**/*.test.{ts,tsx}"],
  },
});
