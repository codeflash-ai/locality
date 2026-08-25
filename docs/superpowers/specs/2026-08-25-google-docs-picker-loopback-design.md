# Google Docs Picker Loopback Design

Google Picker cannot run in Locality's packaged Tauri webview because its iframe RPC implementation accepts only HTTP(S) parent origins, while the packaged application is served from `tauri://localhost`. After a successful Google Docs OAuth connection this currently leaves the Picker dialog blank.

Locality will serve a one-use Picker page from an ephemeral listener bound only to `127.0.0.1`, then open that page in the user's default browser. The page receives the current OAuth token and public Picker configuration only in its HTML response. It initializes Google Picker from an HTTP origin and posts the selected native Google Docs IDs back to the same listener.

Each session has a cryptographically random token. The listener accepts only the matching session path, validates a JSON list of non-empty document IDs, returns a completion page, and sends the IDs to the waiting desktop command. It serves no other routes, persists neither token nor selection, and times out if the user cancels or closes the browser. The desktop front end awaits that command instead of loading Google Picker inside the Tauri window.

The browser Picker remains multi-select and Google-Docs-only. It retains `drive.file` for selected files and files Locality creates; no Drive metadata scope or API integration is introduced.
