import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const cargoToml = readFileSync(join(here, "../src-tauri/Cargo.toml"), "utf8");
const tauriMain = readFileSync(join(here, "../src-tauri/src/main.rs"), "utf8");

describe("desktop single-instance startup", () => {
  it("registers cross-platform single-instance handling before other plugins", () => {
    expect(cargoToml).toMatch(/^tauri-plugin-single-instance = "2"$/m);
    expect(tauriMain).toMatch(
      /tauri::Builder::default\(\)\s*\.plugin\(tauri_plugin_single_instance::init\([\s\S]*?\)\)\s*\.plugin\(tauri_plugin_dialog::init\(\)\)/,
    );
  });

  it("activates the existing window only for foreground launches", () => {
    expect(tauriMain).toMatch(
      /if desktop_second_launch_should_show_main_window\(args\.iter\(\)\) \{\s*show_main_window_with_view\(app, None\);\s*\}/,
    );
  });
});
