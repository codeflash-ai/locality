import { describe, expect, it } from "vitest";
import {
  portableWorkspaceSuccessMessage,
  validatePortableWorkspaceForm,
  workspaceWorkflowCommand,
} from "./portable-workspace";

describe("hosted portable workspace", () => {
  it("builds the exact Desktop command request", () => {
    expect(validatePortableWorkspaceForm({
      apiUrl: " https://workspace.example.test/api ",
      root: " /mnt/locality ",
      profileKey: "a".repeat(64),
    })).toEqual({
      ok: true,
      request: {
        apiUrl: "https://workspace.example.test/api",
        root: "/mnt/locality",
        profileKey: "a".repeat(64),
      },
    });
  });

  it("keeps hosted materialization separate from the existing local mount command", () => {
    expect(workspaceWorkflowCommand("hosted")).toBe("materialize_portable_workspace");
    expect(workspaceWorkflowCommand("local")).toBe("create_workspace_mount");
  });

  it("rejects incomplete or malformed hosted credentials before invoking Tauri", () => {
    expect(validatePortableWorkspaceForm({
      apiUrl: "file:///tmp/workspace",
      root: "/mnt/locality",
      profileKey: "a".repeat(64),
    })).toEqual({ ok: false, message: "The hosted workspace API URL must use HTTP or HTTPS." });
    expect(validatePortableWorkspaceForm({
      apiUrl: "https://workspace.example.test",
      root: "/mnt/locality",
      profileKey: "secret",
    })).toEqual({
      ok: false,
      message: "The Workspace Profile key must be 64 lowercase hexadecimal characters.",
    });
  });

  it("renders completion state without exposing the profile key", () => {
    const message = portableWorkspaceSuccessMessage({
      ok: true,
      root: "/mnt/locality",
      session_id: "session-7",
      content_encoding: "zstd",
      entries: 8,
      files: 4,
      directories: 3,
      materialized_bytes: 120,
      decoded_bytes: 4096,
    });
    expect(message).toBe("Materialized 4 file(s) and 3 folder(s) at /mnt/locality.");
    expect(message).not.toContain("secret");
  });
});
