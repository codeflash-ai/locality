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

export function invokePortableWorkspace(
  invoker: (
    command: string,
    args: { request: PortableWorkspaceRequest },
  ) => Promise<PortableWorkspaceReport>,
  request: PortableWorkspaceRequest,
): Promise<PortableWorkspaceReport> {
  return invoker(workspaceWorkflowCommand("hosted"), { request });
}

export function validatePortableWorkspaceForm(
  form: PortableWorkspaceForm,
): PortableWorkspaceValidation {
  const apiUrl = form.apiUrl;
  const root = form.root;
  const profileKey = form.profileKey;
  if (!apiUrl) {
    return { ok: false, message: "Enter the hosted workspace API URL." };
  }
  try {
    const parsed = new URL(apiUrl);
    if (parsed.protocol !== "https:" && parsed.protocol !== "http:") {
      return { ok: false, message: "The hosted workspace API URL must use HTTP or HTTPS." };
    }
    if (parsed.username || parsed.password) {
      return { ok: false, message: "The hosted workspace API URL must not contain credentials." };
    }
    if (apiUrl.includes("?") || apiUrl.includes("#")) {
      return { ok: false, message: "The hosted workspace API URL must not contain a query or fragment." };
    }
    if (parsed.pathname !== "/") {
      return { ok: false, message: "The hosted workspace API URL must not contain a path." };
    }
    if (parsed.protocol === "http:" && !isLoopbackHostname(parsed.hostname)) {
      return { ok: false, message: "HTTP is allowed only for a loopback hosted workspace." };
    }
  } catch {
    return { ok: false, message: "Enter a valid hosted workspace API URL." };
  }
  if (!isAbsoluteWorkspaceRoot(root)) {
    return { ok: false, message: "The local workspace root must be an absolute path." };
  }
  if (!/^[0-9a-f]{64}$/.test(profileKey)) {
    return { ok: false, message: "The Workspace Profile key must be 64 lowercase hexadecimal characters." };
  }
  return { ok: true, request: { apiUrl, root, profileKey } };
}

function isLoopbackHostname(hostname: string): boolean {
  const normalized = hostname.toLowerCase().replace(/^\[(.*)\]$/, "$1");
  if (normalized === "localhost" || normalized === "::1") {
    return true;
  }
  const octets = normalized.split(".");
  return octets.length === 4
    && octets.every((octet) => /^\d{1,3}$/.test(octet) && Number(octet) <= 255)
    && Number(octets[0]) === 127;
}

function isAbsoluteWorkspaceRoot(root: string): boolean {
  return root.startsWith("/")
    || /^[A-Za-z]:[\\/]/.test(root)
    || /^\\\\[^\\]/.test(root);
}

export function portableWorkspaceSuccessMessage(report: PortableWorkspaceReport): string {
  return `Materialized ${report.files} file(s) and ${report.directories} folder(s) at ${report.root}.`;
}
