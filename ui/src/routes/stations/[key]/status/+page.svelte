<!--
  §0.7 step 3 and §7.1's "Station — Status": a live panel fed by the SSE stream, not a
  poll and not a "Test" button.

  §0.7's diagnostic rule is the part worth getting right: if nothing arrives within 60
  seconds the panel becomes the diagnostic rather than a spinner, because a spinner that
  spins forever is how an operator concludes the product is broken without being told
  what to try.
-->
<script lang="ts">
  import { Box, Dot, Table } from '@chrissnell/chonky-ui';
  import { page } from '$app/state';
  import { common, stations as copy } from '$lib/copy';
  import { ago, compass, measure, stamp } from '$lib/format';
  import { config } from '$lib/live/config.svelte';
  import { stream } from '$lib/sse/stream.svelte';
  import { isStale } from '$lib/sse/machine';
  import StatusBadge from '$lib/components/StatusBadge.svelte';
  import ScheduleLine from '$lib/components/ScheduleLine.svelte';

  const key = $derived(page.params.key ?? '');
  const station = $derived(config.station(key));
  const reading = $derived(stream.readings[key]);
  const stale = $derived(isStale(stream.state));

  const component = $derived(
    stream.components.find((c) => c.display.section === 'stations' && c.id.endsWith(`:${key}`))
  );

  // The 60-second rule. Measured from when this page opened rather than from process
  // start: "nothing has arrived since you started looking" is the operator's question.
  const openedAt = Date.now();
  const waitedLongEnough = $derived(!reading && stream.now - openedAt > 60_000);
</script>

<Box title={copy.status.livePanelHeading}>
  <div class="panelhead">
    <span class="when">
      {#if reading}
        <Dot on={!stale} />
        {copy.status.lastReading(ago(reading.ts, stream.now))}
      {:else}
        {copy.status.noReadingsYet}
      {/if}
    </span>
    {#if component}
      <StatusBadge
        badge={component.badge}
        state={component.state}
        live={Boolean(reading) && !stale}
        sentence={component.sentence}
      />
    {/if}
  </div>

  {#if reading}
    <dl class:stale>
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
      <dt>Solar</dt>
      <dd>{measure(reading.solar_wm2, 'W/m²', 0)}</dd>
      <dt>Rain today</dt>
      <dd>{measure(reading.rain_day_in, 'in', 2)}</dd>
      {#if reading.snow_depth_in !== null && reading.snow_depth_in !== undefined}
        <dt>Snow depth</dt>
        <dd>{measure(reading.snow_depth_in, 'in')}</dd>
      {/if}
    </dl>
    <p class="ok">
      <Dot on={!stale} />
      {stale ? common.offline : copy.status.readingsArriving}
    </p>
  {:else if waitedLongEnough}
    <!-- §0.7: the panel becomes the diagnostic, with what we tried and what to do next. -->
    <p class="diag">{copy.status.noReadingsDiagnostic}</p>
    {#if component?.last_error}
      <p class="diag err">{component.last_error}</p>
    {/if}
    <p>
      <a href={`/stations/${key}/diagnostics`}>{copy.diagnostics.title}</a>
      &middot;
      <a href="/system/activity">{copy.status.checkActivity}</a>
    </p>
  {:else}
    <p class="diag">{common.loading}</p>
  {/if}
</Box>

{#if component}
  <Box title={copy.status.counters}>
    <ScheduleLine schedule={component.schedule} now={stream.now} />
    {#each component.notes as note (note)}
      <p class="note">{note}</p>
    {/each}
    <Table>
      <tbody>
        <tr><td>Readings taken</td><td>{component.counters.runs_total}</td></tr>
        <tr><td>Failures</td><td>{component.counters.failed_total}</td></tr>
        <tr><td>Running since</td><td>{stamp(component.since)}</td></tr>
        <tr>
          <td>Settings in use</td>
          <td>
            {component.pending ? 'Saved, and still catching up.' : 'The ones that are saved.'}
          </td>
        </tr>
      </tbody>
    </Table>
  </Box>
{/if}

{#if station?.shared_with.length}
  <p class="note">Shared with {station.shared_with.length} service(s).</p>
{/if}

<style>
  .panelhead {
    display: flex;
    justify-content: space-between;
    gap: 1rem;
    flex-wrap: wrap;
    margin-block-end: 0.75rem;
  }

  .when {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    font-size: var(--text-xs);
    color: var(--color-text-muted);
  }

  dl {
    display: grid;
    grid-template-columns: auto 1fr auto 1fr;
    gap: 0.25rem 0.75rem;
    margin: 0;
    font-size: var(--text-sm);
  }

  /* §5.2: dim rather than silently current. */
  dl.stale {
    opacity: 0.6;
  }

  dt {
    color: var(--color-text-muted);
  }

  dd {
    margin: 0;
  }

  .ok {
    margin: 0.75rem 0 0;
    display: flex;
    align-items: center;
    gap: 0.4rem;
    font-size: var(--text-sm);
  }

  .diag,
  .note {
    margin: 0.25rem 0;
    font-size: var(--text-sm);
    color: var(--color-text-muted);
    max-width: 70ch;
  }

  .err {
    color: var(--color-danger);
  }

  @media (max-width: 640px) {
    dl {
      grid-template-columns: auto 1fr;
    }
  }
</style>
