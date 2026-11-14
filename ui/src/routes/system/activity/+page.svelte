<!--
  §7.1's "Activity": `LogViewer` over the merged stream, and the one page that subscribes
  to the `logs` channel.

  Four things here are decisions rather than layout:

  * **the two filters are distinct** (§5.5). The view filter is client-side, instant and
    destructive of nothing; the capture level changes what is *recorded* and can destroy
    detail. Conflating them is how an operator loses the error they were hunting.
  * **a standing override is a visible condition**, not a setting -- a TTL-less raise shows
    an amber chip with a [Stop], because debug left on forever quietly fills the ring.
  * **the gap marker is inline** (§5.4), never a toast and never a silent skip, and three
    of them inside a minute adds the sentence that says it is a volume problem.
  * **the buffer is capped at 2,000** (§6.3 gap G8) and the page says so, with a link to
    the paged full log, because `LogViewer` renders every entry it is given.
-->
<script lang="ts">
  import { Badge, Box, Button, Input, LogViewer, Select, toast } from '@chrissnell/chonky-ui';
  import { page } from '$app/state';
  import { api, asFailure, unwrap } from '$lib/api/client';
  import type { Schemas } from '$lib/api/client';
  import { common, system as copy } from '$lib/copy';
  import { clock } from '$lib/format';
  import { BUFFER_CAP, gapBurst, type Row, type ViewFilter } from '$lib/sse/buffer';
  import { config } from '$lib/live/config.svelte';
  import { stream } from '$lib/sse/stream.svelte';
  import { isStale } from '$lib/sse/machine';

  let level = $state<ViewFilter['level']>('info');
  let streamFilter = $state<ViewFilter['stream']>('both');
  let station = $state<string | null>(page.url.searchParams.get('station'));
  let search = $state('');
  let paused = $state(false);
  let follow = $state(true);

  let frozen = $state<Row[]>([]);
  let levels = $state<Schemas['LogLevelsView'] | null>(null);
  let overrideTarget = $state('');
  let overrideLevel = $state('debug');
  let overrideFor = $state('10m');

  $effect(() => {
    void config.load();
    void refreshLevels();
  });

  async function refreshLevels() {
    try {
      levels = unwrap(await api.GET('/api/v1/runtime/log-levels'));
    } catch (e) {
      void asFailure(e);
    }
  }

  const filter = $derived<ViewFilter>({ level, stream: streamFilter, station, search });

  // Pause freezes the list rather than the connection: the records keep arriving and keep
  // being buffered, so resuming shows what happened while the operator was reading.
  const rows = $derived(paused ? frozen : stream.visible(filter));

  function togglePause(next: boolean) {
    if (next) frozen = stream.visible(filter);
    paused = next;
  }

  const entries = $derived(
    rows.map((row) => {
      if (row.kind === 'entry') {
        return {
          kind: 'entry',
          ts: clock(row.ts),
          level: row.entry.level.toLowerCase() as 'info' | 'warn' | 'error' | 'debug',
          where: row.entry.spans.at(-1) ?? row.entry.target,
          message: row.entry.message,
          detail: row.entry.causes.join(' → ')
        };
      }
      if (row.kind === 'gap') {
        return {
          kind: 'gap',
          ts: clock(row.ts),
          level: 'warn' as const,
          where: row.stream,
          message: copy.activity.gap(row.dropped),
          detail: ''
        };
      }
      return {
        kind: 'caught_up',
        ts: clock(row.ts),
        level: 'info' as const,
        where: '',
        message: `— ${copy.activity.caughtUp} —`,
        detail: ''
      };
    })
  );

  const burst = $derived(gapBurst(stream.rows, stream.now));
  const stale = $derived(isStale(stream.state));

  const columns = [
    { key: 'ts', label: copy.activity.columns.ts, width: '9ch' },
    { key: 'level', label: copy.activity.columns.level, width: '6ch' },
    { key: 'where', label: copy.activity.columns.target, width: '22ch' },
    { key: 'message', label: copy.activity.columns.message }
  ];

  const stationOptions = $derived([
    { value: '', label: 'Every station' },
    ...config.stations.map((s) => ({ value: s.key, label: s.name }))
  ]);

  /** §5.5: the target picker lists plain names and maps to a `tracing` target here. */
  const targetOptions = $derived([
    { value: '', label: 'Everything' },
    ...config.stations.map((s) => ({
      value: `mw_station[station="${s.key}"]`,
      label: `${s.name} — ${s.kind_display}`
    })),
    { value: 'mw_output', label: 'Uploads to weather services' },
    { value: 'mw_store', label: 'The weather database' },
    { value: 'mw_api_mgmt', label: 'This admin interface' }
  ]);

  const TTLS: Record<string, string> = {
    '5m': copy.activity.moreDetail.durations['5m'],
    '10m': copy.activity.moreDetail.durations['10m'],
    '30m': copy.activity.moreDetail.durations['30m'],
    '1h': copy.activity.moreDetail.durations['1h'],
    forever: copy.activity.moreDetail.durations.forever
  };

  /**
   * The directive the server installs. `GET /api/v1/runtime/log-levels` reports one
   * directive and `POST /api/v1/runtime/log-level` installs one, so a TTL is expressed by
   * the UI scheduling the revert rather than by the server holding a timer -- which is
   * stated rather than implied, because W3 §5.5 puts the expiry in the supervisor's heap
   * and that half is not on this surface yet. A TTL-less override is therefore the honest
   * default here and the chip below is what keeps it visible.
   */
  async function raise() {
    const directive = overrideTarget
      ? `info,${overrideTarget}=${overrideLevel}`
      : overrideLevel;
    try {
      unwrap(await api.POST('/api/v1/runtime/log-level', { body: { directive } }));
      await refreshLevels();
      toast('Showing extra detail.', 'success');
      if (overrideFor !== 'forever') {
        const ms =
          { '5m': 300_000, '10m': 600_000, '30m': 1_800_000, '1h': 3_600_000 }[overrideFor] ??
          600_000;
        setTimeout(() => void reset(), ms);
      }
    } catch (e) {
      toast(asFailure(e).summary, 'danger');
    }
  }

  async function reset() {
    try {
      unwrap(await api.POST('/api/v1/runtime/log-level', { body: { directive: null } }));
      await refreshLevels();
    } catch (e) {
      toast(asFailure(e).summary, 'danger');
    }
  }

  /** Whether the capture level is above the level the process started with. */
  const standing = $derived(
    Boolean(levels && /=(debug|trace)/.test(levels.directive))
  );

  function download() {
    const text = stream.rows
      .filter((r) => r.kind === 'entry')
      .map((r) =>
        r.kind === 'entry'
          ? `${r.entry.ts} ${r.entry.level} ${r.entry.target} ${r.entry.message}`
          : ''
      )
      .join('\n');
    const url = URL.createObjectURL(new Blob([text], { type: 'text/plain' }));
    const a = document.createElement('a');
    a.href = url;
    a.download = 'activity.log';
    a.click();
    URL.revokeObjectURL(url);
  }
</script>

<h1>{copy.activity.title}</h1>
<p class="blurb">{copy.activity.blurb}</p>

{#if standing}
  <!-- §5.5: a standing override is a condition an operator should see stated. -->
  <p class="standing">
    <Badge variant="warning">Extra detail</Badge>
    {copy.activity.moreDetail.standing(levels?.directive ?? '')}
    <Button variant="ghost" size="sm" onclick={reset}>{copy.activity.moreDetail.stop}</Button>
  </p>
{/if}

<Box>
  <div class="filters">
    <label>
      <span>{copy.activity.filters.level}</span>
      <Select
        value={level}
        onValueChange={(v) => (level = v as ViewFilter['level'])}
        options={[
          { value: 'error', label: copy.activity.levels.error },
          { value: 'warn', label: copy.activity.levels.warn },
          { value: 'info', label: copy.activity.levels.info },
          { value: 'debug', label: copy.activity.levels.debug },
          { value: 'trace', label: copy.activity.levels.trace }
        ]}
      />
    </label>
    <label>
      <span>{copy.activity.filters.stream}</span>
      <Select
        value={streamFilter}
        onValueChange={(v) => (streamFilter = v as ViewFilter['stream'])}
        options={[
          { value: 'both', label: copy.activity.streams.both },
          { value: 'app', label: copy.activity.streams.app },
          { value: 'access', label: copy.activity.streams.access }
        ]}
      />
    </label>
    <label>
      <span>{copy.activity.filters.station}</span>
      <Select
        value={station ?? ''}
        onValueChange={(v) => (station = v || null)}
        options={stationOptions}
      />
    </label>
    <label class="grow">
      <span>{copy.activity.filters.search}</span>
      <Input
        value={search}
        oninput={(e) => (search = (e.currentTarget as HTMLInputElement).value)}
      />
    </label>
    <Button variant="ghost" size="sm" onclick={download}>{copy.activity.download}</Button>
  </div>
  <p class="dim">{copy.activity.moreDetail.viewNote}</p>

  {#if burst}
    <p class="warn">{copy.activity.gapRepeated}</p>
  {/if}
  {#if stale}
    <p class="warn">{common.offline} {common.offlineHint}</p>
  {/if}

  {#if entries.length === 0}
    <p class="dim">{copy.activity.empty.body}</p>
  {:else}
    <LogViewer
      {entries}
      {columns}
      live={stream.state.phase === 'live' && !paused}
      autoscroll={follow}
      height="26rem"
      toolbarToggles={[
        {
          label: copy.activity.paused,
          checked: paused,
          onChange: togglePause,
          title: 'Freeze the list. Records keep arriving.'
        },
        { label: copy.activity.autoscroll, checked: follow, onChange: (v) => (follow = v) }
      ]}
    />
  {/if}

  {#if stream.rows.length >= BUFFER_CAP}
    <p class="dim">
      {copy.activity.bufferCap(BUFFER_CAP)}
      <a href="/api/v1/logs?limit=2000" target="_blank" rel="noreferrer">
        {copy.activity.fullLog}
      </a>
    </p>
  {/if}
</Box>

<Box title={copy.activity.moreDetail.heading}>
  <p class="dim">{copy.activity.moreDetail.captureNote}</p>
  <div class="filters">
    <label>
      <span>{copy.activity.moreDetail.forLabel}</span>
      <Select
        value={overrideTarget}
        onValueChange={(v) => (overrideTarget = v)}
        options={targetOptions}
      />
    </label>
    <label>
      <span>{copy.activity.moreDetail.detailLabel}</span>
      <Select
        value={overrideLevel}
        onValueChange={(v) => (overrideLevel = v)}
        options={[
          { value: 'debug', label: 'Everything (DEBUG)' },
          { value: 'trace', label: 'Everything, including the noise (TRACE)' }
        ]}
      />
    </label>
    <label>
      <span>{copy.activity.moreDetail.durationLabel}</span>
      <Select
        value={overrideFor}
        onValueChange={(v) => (overrideFor = v)}
        options={Object.entries(TTLS).map(([value, label]) => ({ value, label }))}
      />
    </label>
    <Button variant="primary" onclick={raise}>{copy.activity.moreDetail.turnOn}</Button>
  </div>
</Box>

<style>
  h1 {
    margin: 0 0 0.25rem;
    font-size: var(--text-xl);
  }

  .blurb,
  .dim {
    margin: 0 0 0.75rem;
    color: var(--color-text-muted);
    font-size: var(--text-sm);
    max-width: 80ch;
  }

  .warn {
    margin: 0 0 0.5rem;
    color: var(--color-warning);
    font-size: var(--text-sm);
  }

  .standing {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    flex-wrap: wrap;
    margin: 0 0 0.75rem;
    font-size: var(--text-sm);
    color: var(--color-warning);
  }

  .filters {
    display: flex;
    gap: 0.75rem;
    align-items: flex-end;
    flex-wrap: wrap;
    margin-block-end: 0.5rem;
  }

  label {
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
    font-size: var(--text-xs);
    color: var(--color-text-muted);
  }

  label.grow {
    flex: 1 1 12rem;
  }
</style>
