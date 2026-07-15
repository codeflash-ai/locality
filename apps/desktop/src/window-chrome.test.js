import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const tauriMain = readFileSync(join(here, "../src-tauri/src/main.rs"), "utf8");

describe("desktop window chrome", () => {
  it("builds the Windows main window without native decorations", () => {
    const mainWindowBuilder = tauriMain.match(
      /fn build_main_window[\s\S]*?WebviewWindowBuilder::new[\s\S]*?builder\.build\(\)\?;/,
    )?.[0];

    expect(tauriMain).toMatch(
      /#\[cfg\(windows\)\]\s*fn main_window_native_decorations\(\) -> bool \{\s*false\s*\}/,
    );
    expect(tauriMain).toMatch(
      /#\[cfg\(not\(windows\)\)\]\s*fn main_window_native_decorations\(\) -> bool \{\s*true\s*\}/,
    );
    expect(mainWindowBuilder).toContain(".decorations(main_window_native_decorations())");
  });
});
