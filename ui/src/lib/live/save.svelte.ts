import { api, asFailure, unwrap, type ApiFailure } from '../api/client';
import type { Schemas } from '../api/client';
import { phase, type ApplyPhase, type Awaiting } from './applied';
import { stream } from '../sse/stream.svelte';
import { home, list } from '../copy';

/**
 * §2.4's save sequence, in one place: preview, save, watch it become live, offer the one
 * step back when it does not.
 *
 * **Saving applies.** There is no staging area and no Apply button to find: a `PUT` is
 * committed, the supervisor reconciles, and the banner reports *progress* rather than a
 * queue. A staged edit the operator forgets to apply is the same defect as today's silent
 * no-op reached by a different road -- today's lie is "saved means running", a staging
 * model's is "I thought I saved that".
 */
class Saves {
  awaiting = $state<Awaiting | null>(null);
  /** Set when the one permitted revert has been offered and taken. */
  reverted = $state(false);

  get phase(): ApplyPhase {
    return phase(this.awaiting, stream.components, stream.now);
  }

  /** Call after any mutating response. */
  accept(applied: Schemas['Applied']): void {
    this.awaiting = {
      generation: applied.generation,
      affects: applied.affects,
      savedAt: Date.now()
    };
    this.reverted = false;
  }

  clear(): void {
    this.awaiting = null;
  }

  /**
   * §2.4 ⑤'s `Undo save`, which is a real action rather than a hope: every write has a
   * generation and the server keeps the previous committed configuration, so one step
   * back is `POST /api/v1/config/revert`. Scope is deliberately one step, offered only on
   * the failure banner.
   */
  async revert(): Promise<ApiFailure | null> {
    const to = this.awaiting ? this.awaiting.generation - 1 : null;
    if (to === null || to < 0) return null;
    try {
      const applied = unwrap(
        await api.POST('/api/v1/config/revert', { body: { to_generation: to } })
      );
      this.accept(applied);
      this.reverted = true;
      return null;
    } catch (e) {
      return asFailure(e);
    }
  }
}

export const saves = new Saves();

/**
 * One preview call, typed by `Change`'s own tag.
 *
 * `Change` is a tagged union (`{ change: 'put_station', ... }`) and the preview takes a
 * **change** rather than a partial document -- A-M7-1-1 -- which is what makes §8.7's
 * deletion consequences and §2.4's save preview literally the same code path rather than
 * two that agree.
 */
export async function previewChange(change: Schemas['Change']): Promise<Schemas['Applied']> {
  return unwrap(await api.POST('/api/v1/config/preview', { body: change }));
}

/**
 * §2.4 ①'s sentence, built from a preview.
 *
 * Not cosmetic: the preview is `POST /api/v1/config/preview`, which runs the supervisor's
 * own pure `plan()` against the proposed configuration inside a transaction it then rolls
 * back -- so the operator learns "this restarts your APRS beacon" **before** agreeing to
 * it rather than discovering it afterwards. M7-1's done-criterion is that a save's
 * preview names exactly the components that then restart, and it holds because the
 * preview and the save are one code path with one flag between them.
 */
export function previewSentence(applied: Schemas['Applied']): string {
  const d = applied.diff;
  const names = (xs: Schemas['Named'][]) => xs.map((n) => n.display);
  const parts: string[] = [];
  if (d.restart.length > 0) parts.push(home.preview.willRestart(names(d.restart)));
  if (d.start.length > 0) parts.push(home.preview.willStart(names(d.start)));
  if (d.stop.length > 0) parts.push(home.preview.willStop(names(d.stop)));
  if (parts.length === 0) return home.preview.nothingToDo;

  // The stated cost M7-1 names: adding or removing a station or a website rebuilds every
  // component together, because the rollup state's and the snapshot table's key sets are
  // fixed at construction. Saying so in advance is what makes it a stated cost rather
  // than a surprise.
  const touched = d.restart.length + d.start.length + d.stop.length;
  if (d.unchanged === 0 && touched > 3) parts.push(home.preview.everything);
  else if (d.unchanged > 0) parts.push(home.preview.unchanged(d.unchanged));

  return parts.join(' ');
}

/** The §2.4 ④ sentence: which blocked thing is holding the change up, in plain words. */
export function blockedSentence(blocked: Schemas['ComponentView'][]): string {
  const subjects = blocked.map((c) => `${oneLine(c.display)} is ${lower(c.sentence)}`);
  return `${list(subjects)}.`;
}

/** `name`, or `name (qualifier)` -- the one-line form §2.5's `blocking` list uses. */
export function oneLine(display: Schemas['Display']): string {
  return display.qualifier ? `${display.name} (${display.qualifier})` : display.name;
}

/** A §0.4 sentence folded into the middle of another one. */
function lower(sentence: string): string {
  const trimmed = sentence.replace(/\.$/, '');
  return trimmed.charAt(0).toLowerCase() + trimmed.slice(1);
}
