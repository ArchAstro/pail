#!/usr/bin/env node
import { createApp } from "./app.js";
import { execFileSync } from "node:child_process";
import { loadConfig } from "./config.js";

const cfg = loadConfig(process.argv.slice(2));
if (!cfg.repos.length && !cfg.discover.length) {
  console.error(
    "pail-web: no repositories configured.\n" +
      "  Create pail-web.config.json ({\"repos\":[{\"name\":\"app\",\"store\":\"gs://bucket/app\"}]}),\n" +
      "  or set PAIL_WEB_REPOS=\"app=gs://bucket/app\", or run `npm run demo`.",
  );
  process.exit(1);
}
try {
  execFileSync(cfg.pail, ["--version"], { stdio: "ignore" });
} catch {
  console.error(
    `pail-web: cannot run the pail binary "${cfg.pail}".\n` +
      "  Install pail, build this checkout with `cargo build --release`, or set PAIL_BIN (or \"pail\" in the config).",
  );
  process.exit(1);
}
const { app, registry } = createApp(cfg);
const server = app.listen(cfg.port, cfg.host, () => {
  registry.warmAll();
  console.log(`pail-web listening on http://${cfg.host}:${cfg.port}  (${cfg.repos.length} repos${cfg.discover.length ? " + discovery" : ""}, actions ${cfg.actions ? "ON" : "off"})`);
});
for (const sig of ["SIGINT", "SIGTERM"] as const)
  process.on(sig, () => {
    registry.close();
    server.close(() => process.exit(0));
    setTimeout(() => process.exit(0), 500).unref();
  });
