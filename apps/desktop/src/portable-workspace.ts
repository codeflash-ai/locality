export type PortableWorkspaceForm = {
  apiUrl: string;
  root: string;
  profileKey: string;
};

export type PortableWorkspaceRequest = {
  apiUrl: string;
  root: string;
  profileKey: string;
};

export type PortableWorkspaceReport = {
  ok: boolean;
  root: string;
  session_id: string;
  content_encoding: string;
  entries: number;
  files: number;
  directories: number;
  materialized_bytes: number;
  decoded_bytes: number;
};

export type PortableWorkspaceValidation =
  | { ok: true; request: PortableWorkspaceRequest }
  | { ok: false; message: string };

export function workspaceWorkflowCommand(mode: "local" | "hosted"): string {
  return mode === "hosted" ? "materialize_portable_workspace" : "create_workspace_mount";
}

export function validatePortableWorkspaceForm(
  form: PortableWorkspaceForm,
): PortableWorkspaceValidation {
  const apiUrl = form.apiUrl.trim();
  const root = form.root.trim();
  const profileKey = form.profileKey.trim();
  if (!apiUrl) {
    return { ok: false, message: "Enter the hosted workspace API URL." };
  }
  try {
    const parsed = new URL(apiUrl);
    if (parsed.protocol !== "https:" && parsed.protocol !== "http:") {
      return { ok: false, message: "The hosted workspace API URL must use HTTP or HTTPS." };
    }
  } catch {
    return { ok: false, message: "Enter a valid hosted workspace API URL." };
  }
  if (!root) {
    return { ok: false, message: "Choose a local root for the hosted workspace." };
  }
  if (!/^[0-9a-f]{64}$/.test(profileKey)) {
    return { ok: false, message: "The Workspace Profile key must be 64 lowercase hexadecimal characters." };
  }
  return { ok: true, request: { apiUrl, root, profileKey } };
}

export function portableWorkspaceSuccessMessage(report: PortableWorkspaceReport): string {
  return `Materialized ${report.files} file(s) and ${report.directories} folder(s) at ${report.root}.`;
}
