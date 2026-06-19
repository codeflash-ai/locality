/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_AFS_DISTRIBUTION_CHANNEL?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
