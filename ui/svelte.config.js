import adapter from '@sveltejs/adapter-static';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

/**
 * SPA mode, per W5 §6.4: `adapter-static` with a `fallback`, no SSR and no
 * prerendering. The server is a Rust binary serving an embedded bundle; there is no
 * Node runtime in production and introducing one would be a second deployment
 * artefact for a single-operator admin tool.
 */
const config = {
  preprocess: vitePreprocess(),
  kit: {
    adapter: adapter({ fallback: 'index.html', precompress: false, strict: false }),
    appDir: 'assets',
    // The committed TypeScript client lives in `generated/`, outside this project, because
    // nothing hand-written may live there (W3 §1.6 rule 3) and nothing generated may live
    // in `src/`. One alias is the whole of the seam.
    alias: { $mgmt: '../generated/ts/mgmt-api.d.ts' },
    // Hashed filenames under /assets/*, which is what lets `mw-api-mgmt` serve them
    // `immutable` while index.html stays `no-cache` (§6.4). The current UI has neither.
    paths: { relative: false },
    typescript: {
      config: (c) => {
        // `svelte-kit sync` writes .svelte-kit/tsconfig.json with paths relative to
        // itself; nothing here needs overriding, but the hook has to return the config.
        return c;
      }
    }
  }
};

export default config;
