import { api } from '../api/client';
import type { Schemas } from '../api/client';
import { append, matches, type Row, type ViewFilter } from './buffer';
import { delayFor, initial, reduce, type StreamEvent, type StreamState } from './machine';
import type { Channel, ComponentFrame, GapFrame, ReadingFrame } from './wire';

/**
 * The one live connection, and §5.6's "no page polls" as code.
 *
 * One `EventSource` for the whole application, several channels on it. §5.1's argument is
 * the operative one: HTTP/1.1 caps a browser at six connections per origin, and more
 * importantly one connection means **one** reconnect path, **one** backfill path and
 * **one** gap story rather than three that drift apart.
 *
 * `EventSource` reconnects by itself with its own un-tunable interval, which is not
 * §5.2's schedule. So the driver closes it on error and reconnects on its own backoff,
 * and because a manually-created `EventSource` does not send `Last-Event-ID`, the resume
 * point travels as `?last_event_id=`. The server accepts either, so a browser that
 * reconnects on its own behalf still resumes correctly.
 */
class Stream {
  state = $state<StreamState>(initial);

  /** §2.3's component list, keyed for delta application. */
  components = $state<Schemas['ComponentView'][]>([]);
  generation = $state(0);

  /** Latest reading per station key, for the Home tiles and §0.7 step 3. */
  readings = $state<Record<string, ReadingFrame>>({});

  /** The capped log buffer (§6.3 gap G8). */
  rows = $state<Row[]>([]);

  /** Whether the daemon restarted under us -- §5.2: an intended restart is not an error. */
  sawRestart = $state(false);

  #source: EventSource | null = null;
  #timer: ReturnType<typeof setTimeout> | null = null;
  #tick: ReturnType<typeof setInterval> | null = null;
  #channels: Channel[] = ['components'];
  #streamId: string | null = null;
  #stopped = false;
  #now = $state(Date.now());

  get now(): number {
    return this.#now;
  }

  /** The id the server assigned this connection, for the in-stream channel change. */
  get streamId(): string | null {
    return this.#streamId;
  }

  start(channels: Channel[]): void {
    this.#channels = channels;
    this.#stopped = false;
    // One interval for the whole application, driving the `Backoff` countdown and every
    // relative timestamp. §5.6's stated exception: this is rendering, not polling -- it
    // asks the server nothing.
    this.#tick ??= setInterval(() => (this.#now = Date.now()), 1_000);
    this.#open();
  }

  stop(): void {
    this.#stopped = true;
    this.#clearTimer();
    if (this.#tick) {
      clearInterval(this.#tick);
      this.#tick = null;
    }
    this.#source?.close();
    this.#source = null;
    this.#apply({ kind: 'stop' });
  }

  /**
   * §5.1: channel subscription is adjustable in-stream, so navigating from Home to the
   * activity log does not drop and re-establish the connection -- which would mean a
   * fresh backfill and a visible stall on a route change.
   */
  async setChannels(channels: Channel[]): Promise<void> {
    const same =
      channels.length === this.#channels.length && channels.every((c) => this.#channels.includes(c));
    if (same) return;
    this.#channels = channels;
    if (!this.#streamId) {
      this.#reopen();
      return;
    }
    const result = await api.POST('/api/v1/events/{stream_id}/channels', {
      params: { path: { stream_id: this.#streamId } },
      body: { channels }
    });
    // A server that does not know this connection any more (it restarted, or the
    // connection was reaped) is not an error worth showing: re-open with the new channel
    // set, which is what the operator asked for either way.
    if (result.error) this.#reopen();
  }

  retryNow(): void {
    this.#clearTimer();
    this.#apply({ kind: 'retry_now' });
    this.#open();
  }

  clearRows(): void {
    this.rows = [];
  }

  visible(filter: ViewFilter): Row[] {
    return this.rows.filter((r) => matches(r, filter));
  }

  #apply(event: StreamEvent): void {
    this.state = reduce(this.state, event);
  }

  #clearTimer(): void {
    if (this.#timer) {
      clearTimeout(this.#timer);
      this.#timer = null;
    }
  }

  #reopen(): void {
    this.#source?.close();
    this.#source = null;
    this.#open();
  }

  #open(): void {
    if (this.#stopped) return;
    const params = new URLSearchParams({ channels: this.#channels.join(',') });
    if (this.state.lastEventId) params.set('last_event_id', this.state.lastEventId);
    const source = new EventSource(`/api/v1/events?${params.toString()}`);
    this.#source = source;

    source.onopen = () => this.#apply({ kind: 'open' });

    source.addEventListener('hello', (e) => {
      const data = JSON.parse((e as MessageEvent).data) as { stream_id: string; generation: number };
      this.#streamId = data.stream_id;
      // A generation that went **backwards** is a different process answering, which is
      // exactly how §5.2 distinguishes an intended restart from a connection that merely
      // failed. Shown as one INFO line, never as a red banner.
      if (data.generation < this.generation) this.sawRestart = true;
      this.generation = data.generation;
    });

    source.addEventListener('component', (e) => {
      const ev = e as MessageEvent;
      this.#seen(ev.lastEventId);
      const frame = JSON.parse(ev.data) as ComponentFrame;
      if (frame.kind === 'snapshot') {
        this.components = frame.components;
      } else if (frame.kind === 'delta') {
        const i = this.components.findIndex((c) => c.id === frame.component.id);
        if (i >= 0) this.components[i] = frame.component;
        else this.components = [...this.components, frame.component];
      } else {
        this.components = this.components.filter((c) => c.id !== frame.id);
      }
      this.generation = frame.generation;
    });

    source.addEventListener('reading', (e) => {
      const ev = e as MessageEvent;
      this.#seen(ev.lastEventId);
      const frame = JSON.parse(ev.data) as ReadingFrame;
      this.readings = { ...this.readings, [frame.station_key]: frame };
    });

    source.addEventListener('log', (e) => {
      const ev = e as MessageEvent;
      this.#seen(ev.lastEventId);
      const entry = JSON.parse(ev.data) as Schemas['LogEntry'];
      this.rows = append(this.rows, [{ kind: 'entry', ts: entry.ts, entry }]);
    });

    source.addEventListener('gap', (e) => {
      const ev = e as MessageEvent;
      this.#seen(ev.lastEventId);
      const frame = JSON.parse(ev.data) as GapFrame;
      this.rows = append(this.rows, [
        { kind: 'gap', ts: frame.ts, stream: frame.stream, dropped: frame.dropped }
      ]);
    });

    source.addEventListener('caught_up', (e) => {
      const ev = e as MessageEvent;
      this.#seen(ev.lastEventId);
      this.#apply({ kind: 'caught_up' });
      if (this.#channels.includes('logs')) {
        this.rows = append(this.rows, [{ kind: 'caught_up', ts: new Date().toISOString() }]);
      }
    });

    source.addEventListener('heartbeat', (e) => this.#seen((e as MessageEvent).lastEventId));

    source.onerror = () => {
      source.close();
      if (this.#source === source) this.#source = null;
      if (this.#stopped) return;
      const delayMs = delayFor(this.state.attempt);
      this.#apply({ kind: 'error', at: Date.now(), delayMs });
      this.#clearTimer();
      this.#timer = setTimeout(() => {
        this.#apply({ kind: 'retry_now' });
        this.#open();
      }, delayMs);
    };
  }

  #seen(id: string): void {
    if (id) this.#apply({ kind: 'event', id });
  }
}

/**
 * The application's one stream. A module-level singleton rather than a context, because
 * §5.1's "one connection" is a property of the whole page and a context would let a
 * second one exist by accident.
 */
export const stream = new Stream();
