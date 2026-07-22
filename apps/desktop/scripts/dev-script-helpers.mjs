import path from "node:path";

function pathModuleForPlatform(platform) {
  return platform === "win32" ? path.win32 : path.posix;
}

export function npmDevServerCommand({
  platform = process.platform,
  env = process.env,
} = {}) {
  if (platform === "win32") {
    return {
      program: env.ComSpec ?? env.COMSPEC ?? "cmd.exe",
      args: ["/d", "/s", "/c", "npm", "run", "dev"],
    };
  }

  return {
    program: "npm",
    args: ["run", "dev"],
  };
}

export function devSidecarPreparationCommands({
  cargo = process.env.CARGO || "cargo",
  platform = process.platform,
  processExecPath = process.execPath,
  scriptDir,
  workspaceRoot,
}) {
  const platformPath = pathModuleForPlatform(platform);
  const locBinary = platform === "win32" ? "loc.exe" : "loc";

  return [
    {
      name: "build-sidecars",
      program: cargo,
      args: ["build", "-p", "loc-cli", "-p", "localityd"],
      cwd: workspaceRoot,
    },
    {
      name: "stop-daemon",
      program: processExecPath,
      args: [
        platformPath.join(scriptDir, "stop-daemon-for-build.mjs"),
        "--loc",
        platformPath.join(workspaceRoot, "target", "debug", locBinary),
      ],
      cwd: workspaceRoot,
    },
  ];
}
