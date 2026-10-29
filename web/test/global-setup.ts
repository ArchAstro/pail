import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pailBin } from "../scripts/pail-bin.mjs";

// One demo repository for the whole run, built by the real pail binary.
export default function setup() {
  const dir = mkdtempSync(join(tmpdir(), "pail-web-test-"));
  const bin = pailBin();
  execFileSync("node", [join(import.meta.dirname, "..", "scripts", "make-demo.mjs"), dir], { env: { ...process.env, PAIL_BIN: bin }, stdio: "pipe" });
  process.env.PAIL_WEB_TEST_STORE = `file://${join(dir, "store")}`;
  process.env.PAIL_BIN = bin;
  return () => rmSync(dir, { recursive: true, force: true });
}
