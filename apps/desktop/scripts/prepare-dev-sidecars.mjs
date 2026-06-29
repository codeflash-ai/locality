#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

if (process.env.LOCALITY_DESKTOP_SKIP_DEV_SIDECARS === "1") {
  console.log("prepare-dev-sidecars: skipped by LOCALITY_DESKTOP_SKIP_DEV_SIDECARS=1");
  process.exit(0);
}

const scriptDir = dirname(fileURLToPath(import.meta.url));
const workspaceRoot = resolve(scriptDir, "../../..");
const cargo = process.env.CARGO || "cargo";

const result = spawnSync(cargo, ["build", "-p", "loc-cli", "-p", "localityd"], {
  cwd: workspaceRoot,
  env: process.env,
  stdio: "inherit",
});

if (result.error) {
  console.error(`prepare-dev-sidecars: failed to run ${cargo}: ${result.error.message}`);
  process.exit(1);
}

process.exit(result.status ?? 1);
