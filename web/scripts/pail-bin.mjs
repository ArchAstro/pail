import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

/** The pail binary: $PAIL_BIN, a build in this checkout, else `pail` on PATH. */
export function pailBin() {
  if (process.env.PAIL_BIN) return process.env.PAIL_BIN;
  const root = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
  for (const p of ["target/release/pail", "target/debug/pail"]) if (existsSync(join(root, p))) return join(root, p);
  return "pail";
}
