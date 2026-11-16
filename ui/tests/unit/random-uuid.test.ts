import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';

/**
 * The `crypto.randomUUID` fallback, pinned where the theme script is pinned and for the
 * same reason: it is a string in `src/app.html` rather than a module, because it has to be
 * in place before anything that might call it is imported, so no type checker and no
 * component test can see it.
 *
 * What it exists for: `randomUUID` is secure-context only, this UI is served over plain
 * HTTP at a LAN address, and chonky's `toast` calls it. Every save raises a toast *after*
 * its POST has committed, so its absence produced `TypeError: crypto.randomUUID is not a
 * function` on saves that had actually landed (GRA-803).
 *
 * The script is executed here rather than matched against, against a `crypto` that has
 * `getRandomValues` and no `randomUUID` -- which is exactly the browser this is for.
 */
const html = readFileSync(new URL('../../src/app.html', import.meta.url), 'utf8');

/** The second inline script; the first is the theme one, which `theme.test.ts` owns. */
function uuidScript(): string {
  const at = html.indexOf('<script>', html.indexOf('</script>'));
  expect(at, 'the fallback script is gone from src/app.html').toBeGreaterThan(-1);
  return html.slice(at + '<script>'.length, html.indexOf('</script>', at));
}

/** `window` as the insecure-context browser presents it, plus whatever `extra` says. */
function fakeWindow(extra: Record<string, unknown> = {}) {
  const proto = Object.create(null);
  const crypto = Object.create(proto);
  crypto.getRandomValues = (b: Uint8Array) => {
    for (let i = 0; i < b.length; i++) b[i] = Math.floor(Math.random() * 256);
    return b;
  };
  Object.assign(crypto, extra);
  return { crypto };
}

function run(win: { crypto: Record<string, unknown> }) {
  new Function('window', uuidScript())(win);
  return win.crypto;
}

const V4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

describe('the crypto.randomUUID fallback', () => {
  it('installs a version-4 generator when the function is absent', () => {
    const crypto = run(fakeWindow());
    expect(typeof crypto.randomUUID).toBe('function');
    expect((crypto.randomUUID as () => string)()).toMatch(V4);
  });

  it('does not repeat itself', () => {
    const crypto = run(fakeWindow());
    const gen = crypto.randomUUID as () => string;
    const seen = new Set(Array.from({ length: 500 }, () => gen()));
    expect(seen.size).toBe(500);
  });

  it('leaves a real implementation alone', () => {
    // A secure context already has one, and it is the better generator of the two.
    const real = () => 'the-browser-one';
    const crypto = run(fakeWindow({ randomUUID: real }));
    expect(crypto.randomUUID).toBe(real);
  });

  it('falls back to the prototype when the instance refuses the property', () => {
    // Some engines present `crypto` frozen. Non-strict assignment fails silently there,
    // so the script has to check rather than assume.
    const win = fakeWindow();
    Object.freeze(win.crypto);
    const crypto = run(win);
    expect(typeof crypto.randomUUID).toBe('function');
    expect((crypto.randomUUID as () => string)()).toMatch(V4);
  });

  it('does nothing at all without getRandomValues', () => {
    // No source of randomness means no identifier; inventing a predictable one would be
    // worse than the absence this leaves in place.
    const win = { crypto: Object.create(null) as Record<string, unknown> };
    const crypto = run(win);
    expect(crypto.randomUUID).toBeUndefined();
  });
});
