import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "node:path";

// @ts-expect-error process is a nodejs global
const host = process.env.TAURI_DEV_HOST;
// GitHub Pages serves only the public demo from the repository subpath.
// Normal WebUI and Tauri builds intentionally keep Vite's root base.
//
// ⚠️ 这个子路径**必须与 GitHub 仓库名一致**（Pages 地址是
// https://<owner>.github.io/<repo>/）。仓库改名后忘了改这里，页面会白屏：
// index.html 能打开，但它引用的是旧路径下的 assets，全部 404。
// 需要临时覆盖时用 VITE_PAGES_BASE 环境变量。
const pagesDemoBase =
  // @ts-expect-error process is a nodejs global
  process.env.VITE_PAGES_BASE || "/WorkBuddy-Switch2api/";
// @ts-expect-error process is a nodejs global
const base = process.env.VITE_PAGES_DEMO === "1" ? pagesDemoBase : "/";

// https://vite.dev/config/
export default defineConfig(async () => ({
  base,
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
