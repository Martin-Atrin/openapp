import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";

/** Serves dev-mock/panel.html at /mock/panel.html in dev only (used by the browser mock kernel); never bundled. */
function devMockPanel(): Plugin {
  return {
    name: "openapp-dev-mock-panel",
    apply: "serve",
    configureServer(server) {
      server.middlewares.use("/mock/panel.html", async (_req, res) => {
        try {
          const html = await readFile(fileURLToPath(new URL("./dev-mock/panel.html", import.meta.url)), "utf8");
          res.setHeader("content-type", "text/html; charset=utf-8");
          res.end(html);
        } catch {
          res.statusCode = 404;
          res.end("not found");
        }
      });
    },
  };
}

export default defineConfig({
  plugins: [react(), devMockPanel()],
  clearScreen: false,
  build: { chunkSizeWarningLimit: 1200 }, // the full lucide icon map is one lazy chunk (icons are looked up by name at runtime)
  server: { port: 5173, strictPort: true, watch: { ignored: ["**/src-tauri/**"] } },
});
