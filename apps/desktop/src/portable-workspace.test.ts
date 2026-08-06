import { describe, expect, it } from "vitest";
import {
  hostedWorkspaceCommand,
  invokeHostedWorkspace,
  invokeHostedWorkspaceList,
  invokePortableWorkspace,
  portableWorkspaceSuccessMessage,
  validatePortableWorkspaceForm,
  workspaceWorkflowCommand,
} from "./portable-workspace";

describe("hosted portable workspace", () => {
  it("builds the exact attach request without a parallel materializer command", async () => {
    const validation = validatePortableWorkspaceForm({
      apiUrl: "https://workspace.example.test",
      root: "/mnt/locality",
      credentialRef: "hosted-workspace:desktop-team",
      profileKey: "a".repeat(64),
    });
    expect(validation).toEqual({
      ok: true,
      request: {
        apiUrl: "https://workspace.example.test",
        root: "/mnt/locality",
        credentialRef: "hosted-workspace:desktop-team",
        profileKey: "a".repeat(64),
      },
    });
    if (!validation.ok) throw new Error("expected valid request");

    const calls: unknown[] = [];
    await invokePortableWorkspace(async (command, args) => {
      calls.push({ command, args });
      return report();
    }, validation.request);
    expect(calls).toEqual([{
      command: "attach_hosted_workspace",
      args: { request: validation.request },
    }]);
    expect(workspaceWorkflowCommand("hosted")).toBe("attach_hosted_workspace");
    expect(workspaceWorkflowCommand("local")).toBe("create_workspace_mount");
  });

  it("maps attach, refresh, relocate, and list to the shared coordinator IPC contract", async () => {
    expect(hostedWorkspaceCommand("attach")).toBe("attach_hosted_workspace");
    expect(hostedWorkspaceCommand("refresh")).toBe("refresh_hosted_workspace");
    expect(hostedWorkspaceCommand("relocate")).toBe("relocate_hosted_workspace");

    const request = {
      apiUrl: "https://workspace.example.test",
      root: "/mnt/relocated",
      credentialRef: "hosted-workspace:desktop-team",
    };
    const calls: unknown[] = [];
    await invokeHostedWorkspace(async (command, args) => {
      calls.push({ command, args });
      return report();
    }, "refresh", request);
    await invokeHostedWorkspace(async (command, args) => {
      calls.push({ command, args });
      return report();
    }, "relocate", request);
    await invokeHostedWorkspaceList(async (command) => {
      calls.push({ command });
      return { ok: true, attachments: [] };
    });
    expect(calls).toEqual([
      { command: "refresh_hosted_workspace", args: { request } },
      { command: "relocate_hosted_workspace", args: { request } },
      { command: "list_hosted_workspaces" },
    ]);
  });

  it("rejects malformed placement, references, and credentials before invoking Tauri", () => {
    const base = {
      apiUrl: "https://workspace.example.test",
      root: "/mnt/locality",
      credentialRef: "hosted-workspace:desktop-team",
      profileKey: "a".repeat(64),
    };
    expect(validatePortableWorkspaceForm({ ...base, credentialRef: "plain-ref" })).toEqual({
      ok: false,
      message: "Enter a valid hosted-workspace credential reference.",
    });
    expect(validatePortableWorkspaceForm({ ...base, profileKey: "secret" })).toEqual({
      ok: false,
      message: "The Workspace Profile key must be 64 lowercase hexadecimal characters.",
    });
    expect(validatePortableWorkspaceForm({ ...base, root: "relative/Locality" })).toEqual({
      ok: false,
      message: "The local workspace root must be an absolute path.",
    });
  });

  it.each([
    ["https://workspace.example.test/api", "The hosted workspace API URL must not contain a path."],
    ["https://user@workspace.example.test", "The hosted workspace API URL must not contain credentials."],
    ["https://workspace.example.test?tenant=7", "The hosted workspace API URL must not contain a query or fragment."],
    ["http://workspace.example.test", "HTTP is allowed only for a loopback hosted workspace."],
  ])("matches Rust URL rejection for %s", (apiUrl, message) => {
    expect(validatePortableWorkspaceForm({
      apiUrl,
      root: "/mnt/locality",
      credentialRef: "hosted-workspace:desktop-team",
      profileKey: "a".repeat(64),
    })).toEqual({ ok: false, message });
  });

  it("renders completion state without exposing the profile key", () => {
    const message = portableWorkspaceSuccessMessage(report());
    expect(message).toBe("Attached 4 file(s) and 3 folder(s) at /mnt/locality.");
    expect(message).not.toContain("secret");
  });
});

function report() {
  return {
    ok: true,
    api_origin: "https://workspace.example.test",
    profile_id: "018f4f6e-9f2c-7b1a-8c3d-4e5f60718293",
    profile_revision: 7,
    root: "/mnt/locality",
    mount_count: 2,
    files: 4,
    directories: 3,
    materialized_bytes: 120,
  };
}
