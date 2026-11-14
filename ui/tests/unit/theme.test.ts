import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';

/**
 * The theme rule, pinned at the only place it can be checked without a browser: the
 * inline boot script in `src/app.html`.
 *
 * It is a string in a document rather than a module because it has to run **before the
 * first paint** -- a module script would paint one theme and then flip. That makes it the
 * one piece of this application no type checker and no component test can see, which is
 * exactly why it shipped wrong.
 *
 * What it got wrong, and what this asserts it does not get wrong again:
 *
 *  1. It wrote `data-theme` unconditionally, resolving the device preference itself. The
 *     attribute **pins** the answer -- chonky's `tokens.css` tracks the device with
 *     `@media (prefers-color-scheme: dark) { :root:not([data-theme="light"]) }`, and that
 *     media query can never apply once an attribute is present. A light laptop got a dark
 *     UI and the device setting stopped meaning anything.
 *  2. Its `catch` branch defaulted to `dark`, so a browser with storage denied was dark
 *     regardless of its device.
 *  3. It used a storage key of our own while chonky's `ThemeToggle` reads and writes
 *     `theme`, so the operator's explicit choice was invisible on the next load.
 */
const html = readFileSync(new URL('../../src/app.html', import.meta.url), 'utf8');
const script = html.slice(html.indexOf('<script>'), html.indexOf('</script>'));

describe('the theme boot script', () => {
  it('reads the key chonky itself writes', () => {
    expect(script).toContain("localStorage.getItem('theme')");
    expect(script, 'a key of our own is a choice the toggle cannot see').not.toMatch(
      /chonky-theme/
    );
  });

  it('writes an attribute only for an explicit choice', () => {
    // Every `setAttribute` in the script must be inside the branch that tested the stored
    // value. Asserted structurally rather than by eye: the guard has to be above it.
    const guard = script.indexOf("stored === 'dark'");
    const writes = [...script.matchAll(/setAttribute\(/g)].map((m) => m.index ?? -1);
    expect(guard, 'the script no longer tests the stored value').toBeGreaterThan(-1);
    expect(writes.length, 'the script writes no attribute at all').toBeGreaterThan(0);
    for (const at of writes) {
      expect(at, 'an attribute is written outside the stored-choice branch').toBeGreaterThan(
        guard
      );
    }
  });

  it('writes nothing when storage is denied', () => {
    // The `catch` must not fall back to a theme: no attribute means the device decides,
    // which is the better failure than defaulting everyone to one of them.
    const tail = script.slice(script.indexOf('catch'));
    expect(tail).not.toContain('setAttribute');
  });

  it('never hard-codes a theme as the answer', () => {
    expect(script).not.toMatch(/setAttribute\(\s*'data-theme'\s*,\s*'(dark|light)'\s*\)/);
  });
});
