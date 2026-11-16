import js from '@eslint/js';
import svelte from 'eslint-plugin-svelte';
import ts from 'typescript-eslint';
import globals from 'globals';

/**
 * Proportionate, matching §6.7: the high-value check is `svelte-check`, which is the API
 * drift gate. This catches the things a type checker does not -- an unused import, an
 * accessibility attribute missing from a control, a `{#each}` with no key.
 */
export default [
  js.configs.recommended,
  ...ts.configs.recommended,
  ...svelte.configs['flat/recommended'],
  {
    languageOptions: {
      globals: { ...globals.browser, ...globals.node }
    }
  },
  {
    files: ['**/*.svelte'],
    languageOptions: {
      parserOptions: { parser: ts.parser }
    }
  },
  {
    rules: {
      // The generated client's types use `any` in two places that are not ours to fix.
      '@typescript-eslint/no-explicit-any': 'off',
      // `$props()` destructuring and `$state` declarations trip the base rule.
      'no-undef': 'off'
    }
  },
  {
    // Playwright detects a fixture's dependencies from its destructuring pattern, so a
    // fixture that depends on nothing is written `async ({}, use)`. That is the framework's
    // documented form rather than an oversight, and the alternative -- `async (_, use)` --
    // makes Playwright treat the fixture as depending on every other one.
    files: ['tests/e2e/**/*.ts'],
    rules: { 'no-empty-pattern': 'off' }
  },
  {
    ignores: ['build/', '.svelte-kit/', 'node_modules/', 'test-results/', 'playwright-report/']
  }
];
