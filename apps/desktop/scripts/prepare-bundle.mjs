#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));

const commands = {
  darwin: ["bash", [join(scriptDir, "prepare-macos-file-provider.sh")]],
  linux: ["bash", [join(scriptDir, "prepare-linux-bundle.sh")]],
  win32: [
    "powershell.exe",
    [
      "-NoProfile",
      "-ExecutionPolicy",
      "Bypass",
      "-File",
      join(scriptDir, "prepare-windows-bundle.ps1"),
    ],
  ],
};

const command = commands[process.platform];
if (!command) {
  console.error(`prepare-bundle: unsupported platform: ${process.platform}`);
  process.exit(1);
}

const [program, args] = command;
const result = spawnSync(program, args, { stdio: "inherit" });
if (result.error) {
  console.error(`prepare-bundle: failed to run ${program}: ${result.error.message}`);
  process.exit(1);
}
process.exit(result.status ?? 1);
