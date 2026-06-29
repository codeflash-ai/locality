#!/usr/bin/env node
import { spawn, spawnSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const desktopDir = resolve(scriptDir, "..");

const prepare = spawnSync(process.execPath, [join(scriptDir, "prepare-dev-sidecars.mjs")], {
  cwd: desktopDir,
  env: process.env,
  stdio: "inherit",
});

if (prepare.error) {
  console.error(`dev-command: failed to prepare sidecars: ${prepare.error.message}`);
  process.exit(1);
}
if ((prepare.status ?? 1) !== 0) {
  process.exit(prepare.status ?? 1);
}

const npm = process.platform === "win32" ? "npm.cmd" : "npm";
const dev = spawn(npm, ["run", "dev"], {
  cwd: desktopDir,
  env: process.env,
  stdio: "inherit",
});

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    dev.kill(signal);
  });
}

dev.on("exit", (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code ?? 1);
});

dev.on("error", (error) => {
  console.error(`dev-command: failed to start npm dev server: ${error.message}`);
  process.exit(1);
});
