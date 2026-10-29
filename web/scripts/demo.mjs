#!/usr/bin/env node
// `npm run demo`: build the demo repository once (needs the pail binary), then start pail-web on it.
import { spawn, execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { pailBin } from "./pail-bin.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const demo = join(root, ".demo");
const bin = pailBin();

try {
  execFileSync(bin, ["--version"], { stdio: "pipe" });
} catch {
  console.error(`demo: cannot run "${bin}". Install pail (https://pail.build), build this checkout with \`cargo build --release\`, or set PAIL_BIN.`);
  process.exit(1);
}
if (!existsSync(join(demo, "pail-web.config.json")) || process.argv.includes("--fresh")) {
  console.log("building the demo repository with", bin);
  execFileSync("node", [join(root, "scripts", "make-demo.mjs"), demo], { stdio: "inherit", env: { ...process.env, PAIL_BIN: bin } });
}
if (!existsSync(join(root, "dist", "public", "index.html"))) {
  console.log("building the UI");
  execFileSync("npm", ["run", "build"], { cwd: root, stdio: "inherit" });
}
const env = { ...process.env, PAIL_BIN: bin, PAIL_WEB_CONFIG: join(demo, "pail-web.config.json") };
const child = spawn("node", [join(root, "dist", "server", "index.js")], { stdio: "inherit", env });
child.on("exit", (code) => process.exit(code ?? 0));
for (const sig of ["SIGINT", "SIGTERM"]) process.on(sig, () => child.kill(sig));
