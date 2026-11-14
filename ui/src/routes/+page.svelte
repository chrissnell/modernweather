<!--
  Home: §2.5's root-cause panel, the counts, one tile per station, and the §0.7 checklist.

  Home is the one navigation item §0.6 adds to the prompt's five, and it is load-bearing
  rather than decorative: §2 is about making "is this actually working?" answerable, and
  that question needs a place to be answered on arrival.
-->
<script lang="ts">
  import { Badge, Box, Dot, EmptyState, StatCard } from '@chrissnell/chonky-ui';
  import { home, stations as stationsCopy } from '$lib/copy';
  import { ago, compass, measure } from '$lib/format';
  import { config, summary } from '$lib/live/config.svelte';
  import { stream } from '$lib/sse/stream.svelte';
  import { session } from '$lib/live/session.svelte';
  import { steps } from '$lib/live/firstrun';
  import FirstRunChecklist from '$lib/components/FirstRunChecklist.svelte';
  import { isStale } from '$lib/sse/machine';

  const DISMISS_KEY = 'mw-firstrun-dismissed';

  let dismissed = $state(false);

  $effect(() => {
    void config.load();
    void summary.load();
    try {
      dismissed = localStorage.getItem(DISMISS_KEY) === '1';
    } catch {
      dismissed = false;
    }
  });

  const checklist = $derived(
    steps({
      claimed: session.claimed,
      stations: config.stations,
      readingStationKeys: Object.keys(stream.readings),
      hasStorage: config.storage !== null,
      websites: config.websites,
      services: config.services
    })
  );

  const counts = $derived(summary.value?.counts ?? null);
  const roots = $derived(summary.value?.root_causes ?? []);
  const stale = $derived(isStale(stream.state));

  function dismiss() {
    dismissed = true;
    try {
      localStorage.setItem(DISMISS_KEY, '1');
    } catch {
      // Hiding a finished checklist is a preference, not data. A browser that refuses
      // storage simply shows it again next time.
    }
  }
</script>

<h1>{home.title}</h1>
<p class="blurb">{home.blurb}</p>

<FirstRunChecklist steps={checklist} {dismissed} ondismiss={dismiss} />

{#if roots.length > 0}
  <Box title={home.attention.heading}>
    {#each roots as root (root.id)}
      <div class="root">
        <p class="headline">
          <Badge variant="danger">{root.display}</Badge>
          {root.message}
        </p>
        {#if root.blocking.length > 0}
          <p class="because">{home.attention.because(root.blocking.length)}</p>
          <ul>
            {#each root.blocking as b (b.href + b.display)}
              <li><a href={b.href}>{b.display}</a></li>
            {/each}
          </ul>
        {/if}
        {#if root.unaffected_note}
          <!--
            Not reassurance padding: W3 §2a.6's measured result is that `/latest` is served
            from the in-memory snapshot and is untouched by a database outage, and this is
            exactly the thing an operator cannot currently determine during an incident.
          -->
          <p class="unaffected">{root.unaffected_note}</p>
        {/if}
        <a class="btn btn-sm btn-ghost" href="/system/diagnostics">
          {home.attention.details}
        </a>
      </div>
    {/each}
  </Box>
{:else if counts}
  <Box title={home.allWell.heading}>
    <p class="blurb">{home.allWell.body}</p>
  </Box>
{/if}

{#if counts}
  <div class="counts">
    <StatCard label={home.counts.running} value={counts.running} variant="success" />
    <StatCard label={home.counts.waiting} value={counts.waiting} variant="info" />
    <StatCard label={home.counts.failed} value={counts.failed} variant="danger" />
    <StatCard label={home.counts.backoff} value={counts.backoff} variant="warning" />
    <StatCard label={home.counts.off} value={counts.off} variant="default" />
  </div>
{/if}

<Box title={home.stations.heading}>
  {#if config.stations.length === 0}
    <EmptyState>
      <h3>{stationsCopy.list.empty.heading}</h3>
      <p>{stationsCopy.list.empty.body}</p>
      <a class="btn btn-primary" href="/stations/new">{stationsCopy.list.empty.action}</a>
    </EmptyState>
  {:else}
    <div class="tiles" class:stale>
      {#each config.stations as station (station.key)}
        {@const reading = stream.readings[station.key]}
        <article>
          <header>
            <a href={`/stations/${station.key}`}>{station.name}</a>
            <span class="when">
              {#if reading}
                <Dot on={!stale} />
                {ago(reading.ts, stream.now)}
              {:else}
                {home.stations.noReading}
              {/if}
            </span>
          </header>
          {#if reading}
            <dl>
              <dt>Temperature</dt>
              <dd>{measure(reading.temperature_f, '°F')}</dd>
              <dt>Humidity</dt>
              <dd>{measure(reading.humidity_pct, '%', 0)}</dd>
              <dt>Pressure</dt>
              <dd>{measure(reading.barometer_inhg, 'inHg', 2)}</dd>
              <dt>Wind</dt>
              <dd>
                {measure(reading.wind_speed_mph, 'mph', 0)}
                {#if reading.wind_dir_degrees !== null && reading.wind_dir_degrees !== undefined}
                  from the {compass(reading.wind_dir_degrees)}
                {/if}
              </dd>
            </dl>
          {/if}
        </article>
      {/each}
    </div>
  {/if}
</Box>

<style>
  h1 {
    margin: 0 0 0.25rem;
    font-size: var(--text-xl);
  }

  .blurb {
    margin: 0 0 1rem;
    color: var(--color-text-muted);
    font-size: var(--text-sm);
  }

  .counts {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(9rem, 1fr));
    gap: 0.75rem;
    margin-block: 1rem;
  }

  .root + .root {
    margin-block-start: 1rem;
    padding-block-start: 1rem;
    border-block-start: 1px solid var(--color-border-subtle);
  }

  .headline {
    margin: 0 0 0.5rem;
    display: flex;
    gap: 0.5rem;
    align-items: center;
    flex-wrap: wrap;
  }

  .because,
  .unaffected {
    margin: 0.25rem 0;
    font-size: var(--text-sm);
    color: var(--color-text-muted);
  }

  ul {
    margin: 0.25rem 0 0.5rem;
    padding-inline-start: 1.2rem;
    font-size: var(--text-sm);
  }

  .tiles {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(16rem, 1fr));
    gap: 0.75rem;
  }

  /* §5.2: any panel fed by the stream dims while disconnected. It never shows stale data
     as though it were live. */
  .tiles.stale {
    opacity: 0.6;
  }

  article {
    border: 1px solid var(--color-border);
    border-radius: var(--radius);
    padding: 0.75rem;
    background: var(--color-surface);
  }

  header {
    display: flex;
    justify-content: space-between;
    gap: 0.5rem;
    align-items: baseline;
    margin-block-end: 0.5rem;
  }

  .when {
    font-size: var(--text-xs);
    color: var(--color-text-muted);
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
  }

  dl {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 0.15rem 0.75rem;
    margin: 0;
    font-size: var(--text-sm);
  }

  dt {
    color: var(--color-text-muted);
  }

  dd {
    margin: 0;
  }
</style>
