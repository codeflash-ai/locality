/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_LOCALITY_DISTRIBUTION_CHANNEL?: string;
  readonly VITE_LOCALITY_ONBOARDING_DEMO_VIDEO_URL?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
