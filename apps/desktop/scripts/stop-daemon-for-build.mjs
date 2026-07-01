#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";

if (process.env.LOCALITY_DESKTOP_SKIP_DAEMON_STOP_FOR_BUILD === "1") {
  console.log(
    "stop-daemon-for-build: skipped by LOCALITY_DESKTOP_SKIP_DAEMON_STOP_FOR_BUILD=1",
  );
  process.exit(0);
}

const args = process.argv.slice(2);
const locFlagIndex = args.indexOf("--loc");
const locPath = locFlagIndex >= 0 ? args[locFlagIndex + 1] : undefined;

function run(program, commandArgs, options = {}) {
  const result = spawnSync(program, commandArgs, {
    env: process.env,
    stdio: options.stdio ?? "ignore",
  });
  if (result.error) {
    return { ok: false, message: result.error.message };
  }
  return { ok: result.status === 0, status: result.status };
}

if (locPath && existsSync(locPath)) {
  const stopped = run(locPath, ["daemon", "stop"], { stdio: "inherit" });
  if (!stopped.ok && stopped.status !== 0) {
    console.log("stop-daemon-for-build: managed daemon stop did not complete; forcing process stop");
  }
} else {
  console.log("stop-daemon-for-build: loc binary not found; forcing process stop only");
}

if (process.platform === "win32") {
  run("taskkill", ["/IM", "localityd.exe", "/F", "/T"]);
} else if (process.platform === "darwin" || process.platform === "linux") {
  run("pkill", ["-x", "localityd"]);
}
