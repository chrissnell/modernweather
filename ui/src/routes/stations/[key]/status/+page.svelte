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
  import { api, asFailure, unwrap, type Schemas } from '$lib/api/client';
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

  // **What the gauge itself reports**, read when this page opens rather than polled: the
  // calibration is a number somebody sets at the device and then leaves alone, and the
  // gauge serves only seven sockets. `Read it again` is there for the one moment it does
  // change -- just after a calibration.
  let device = $state<Schemas['GaugeDeviceView'] | null>(null);
  let deviceError = $state<string | null>(null);
  let deviceLoading = $state(false);

  async function readDevice(): Promise<void> {
    deviceLoading = true;
    try {
      device = unwrap(
        await api.GET('/api/v1/stations/{key}/device', { params: { path: { key } } })
      );
      deviceError = null;
    } catch (e) {
      device = null;
      deviceError = asFailure(e).summary;
    } finally {
      deviceLoading = false;
    }
  }

  $effect(() => {
    if (station?.kind === 'snowgauge' && device === null && deviceError === null && !deviceLoading) {
      void readDevice();
    }
  });
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

{#if station?.kind === 'snowgauge'}
  <Box title={copy.status.device.heading}>
    <p class="note">{copy.status.device.intro}</p>
    {#if deviceError}
      <p class="diag err">{deviceError}</p>
    {:else if device && !device.reachable}
      <p class="diag">
        {copy.status.device.unreachable}
        {#if device.unreachable_because}{device.unreachable_because}{/if}
      </p>
    {:else if device}
      <Table>
        <tbody>
          <tr>
            <td>{copy.status.device.baseDistance}</td>
            <td>
              {#if device.base_distance_mm !== undefined}
                {device.base_distance_mm} mm
              {:else}
                {copy.status.device.notCalibrated}
              {/if}
            </td>
          </tr>
          {#if device.snow_depth_mm !== undefined}
            <tr><td>{copy.status.device.depth}</td><td>{device.snow_depth_mm} mm</td></tr>
          {/if}
          <tr>
            <td>{copy.status.device.calibration}</td>
            <td>
              {device.calibration}{#if device.calibration_detail}
                &nbsp;&mdash; {device.calibration_detail}
              {/if}
            </td>
          </tr>
          {#if device.firmware_version}
            <tr><td>{copy.status.device.firmware}</td><td>{device.firmware_version}</td></tr>
          {/if}
          {#if device.firmware_slot}
            <tr><td>{copy.status.device.slot}</td><td>{device.firmware_slot}</td></tr>
          {/if}
          {#if device.rssi_dbm !== undefined}
            <tr><td>{copy.status.device.signal}</td><td>{device.rssi_dbm} dBm</td></tr>
          {/if}
        </tbody>
      </Table>
      {#if device.base_distance_mm === undefined}
        <p class="note">{copy.status.device.calibrateHint}</p>
      {/if}
    {/if}
    <button type="button" onclick={() => void readDevice()} disabled={deviceLoading}>
      {copy.status.device.reread}
    </button>
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
