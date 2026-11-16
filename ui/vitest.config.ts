import { defineConfig } from 'vitest/config';

/**
 * Separate from `vite.config.ts` on purpose: the SvelteKit plugin wants a `.svelte-kit`
 * sync to have run and the unit tests are over plain TypeScript modules (§6.7 -- the
 * vocabulary mapping, the `applied_generation` derivation, the SSE state machine and the
 * status renderer), so they are the four things worth testing in isolation and none of
 * them needs the framework.
 */
export default defineConfig({
  test: {
    environment: 'node',
    include: ['tests/unit/**/*.test.ts']
  }
});
