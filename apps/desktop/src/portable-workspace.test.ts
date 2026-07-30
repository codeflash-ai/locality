import { describe, expect, it } from "vitest";
import {
  invokePortableWorkspace,
  portableWorkspaceSuccessMessage,
  validatePortableWorkspaceForm,
  workspaceWorkflowCommand,
} from "./portable-workspace";

describe("hosted portable workspace", () => {
  it("builds the exact Desktop command request", async () => {
    expect(validatePortableWorkspaceForm({
      apiUrl: "https://workspace.example.test",
      root: "/mnt/locality",
      profileKey: "a".repeat(64),
    })).toEqual({
      ok: true,
      request: {
        apiUrl: "https://workspace.example.test",
        root: "/mnt/locality",
        profileKey: "a".repeat(64),
      },
    });

    const calls: unknown[] = [];
    const request = {
      apiUrl: "https://workspace.example.test",
      root: "/mnt/locality",
      profileKey: "a".repeat(64),
    };
    await invokePortableWorkspace(async (command, args) => {
      calls.push({ command, args });
      return report();
    }, request);
    expect(calls).toEqual([{
      command: "materialize_portable_workspace",
      args: { request },
    }]);
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

  it.each([
    ["https://workspace.example.test/api", "The hosted workspace API URL must not contain a path."],
    ["https://user@workspace.example.test", "The hosted workspace API URL must not contain credentials."],
    ["https://workspace.example.test?tenant=7", "The hosted workspace API URL must not contain a query or fragment."],
    ["https://workspace.example.test#tenant", "The hosted workspace API URL must not contain a query or fragment."],
    ["http://workspace.example.test", "HTTP is allowed only for a loopback hosted workspace."],
    ["http://127.1", "HTTP is allowed only for a loopback hosted workspace."],
  ])("matches Rust URL rejection for %s", (apiUrl, message) => {
    expect(validatePortableWorkspaceForm({
      apiUrl,
      root: "/mnt/locality",
      profileKey: "a".repeat(64),
    })).toEqual({ ok: false, message });
  });

  it("accepts loopback HTTP and rejects relative roots", () => {
    expect(validatePortableWorkspaceForm({
      apiUrl: "http://127.0.0.1:8080",
      root: "/mnt/locality",
      profileKey: "a".repeat(64),
    }).ok).toBe(true);
    expect(validatePortableWorkspaceForm({
      apiUrl: "https://workspace.example.test",
      root: "relative/Locality",
      profileKey: "a".repeat(64),
    })).toEqual({ ok: false, message: "The local workspace root must be an absolute path." });
  });

  it("renders completion state without exposing the profile key", () => {
    const message = portableWorkspaceSuccessMessage(report());
    expect(message).toBe("Materialized 4 file(s) and 3 folder(s) at /mnt/locality.");
    expect(message).not.toContain("secret");
  });
});

function report() {
  return {
    ok: true,
    root: "/mnt/locality",
    session_id: "session-7",
    content_encoding: "zstd",
    entries: 8,
    files: 4,
    directories: 3,
    materialized_bytes: 120,
    decoded_bytes: 4096,
  };
}
