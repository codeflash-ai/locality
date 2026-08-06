import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const cargoToml = readFileSync(join(here, "../src-tauri/Cargo.toml"), "utf8");
const tauriMain = readFileSync(join(here, "../src-tauri/src/main.rs"), "utf8");
const singleInstance = readFileSync(
  join(here, "../src-tauri/src/single_instance.rs"),
  "utf8",
);
const macosAppEntitlements = readFileSync(
  join(
    here,
    "../../../platform/macos/LocalityFileProvider/App/Locality.entitlements",
  ),
  "utf8",
);

describe("desktop single-instance startup", () => {
  it("claims ownership before Tauri setup and bypasses forwarding for release smoke", () => {
    expect(cargoToml).not.toMatch(/^tauri-plugin-single-instance\s*=/m);
    const smokeCheck = tauriMain.indexOf(
      "desktop_single_instance_required(smoke_test_requested)",
    );
    const ownershipClaim = tauriMain.indexOf(
      "single_instance::acquire_desktop_single_instance(background_launch)",
    );
    const tauriBuilder = tauriMain.indexOf("tauri::Builder::default()");
    expect(smokeCheck).toBeGreaterThan(-1);
    expect(ownershipClaim).toBeGreaterThan(smokeCheck);
    expect(tauriBuilder).toBeGreaterThan(ownershipClaim);
  });

  it("uses atomic exclusion and a sandbox-safe macOS endpoint", () => {
    expect(singleInstance).toContain("libc::LOCK_EX | libc::LOCK_NB");
    expect(singleInstance).toContain("libc::_CS_DARWIN_USER_TEMP_DIR");
    expect(singleInstance).toContain('user_temp_dir.join("locality-si")');
    expect(singleInstance).toContain("DARWIN_UNIX_SOCKET_PATH_MAX");
    expect(singleInstance).not.toContain('Path::new("/tmp")');
    expect(macosAppEntitlements).toMatch(
      /<key>com\.apple\.security\.app-sandbox<\/key>\s*<true\/>/,
    );
    expect(singleInstance.indexOf("try_lock_coordination_file(&lock_file)")).toBeLessThan(
      singleInstance.indexOf("UnixListener::bind(&activation_socket_path)"),
    );
  });
});
