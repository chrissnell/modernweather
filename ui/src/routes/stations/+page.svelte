<!--
  §7.1's station list. The one editable thing here is the inline "Collecting readings"
  toggle, which is a configuration change like any other: it is saved, previewed and
  reported on, which is what makes it survive a restart (Decision 22).

  §0.6: stations received from a peer appear here too, badged, and read-only -- this is
  where an operator looks for "what stations do I have", and the editing lives where the
  pairing is configured.
-->
<script lang="ts">
  import { Badge, Box, EmptyState, Table, Toggle, toast } from '@chrissnell/chonky-ui';
  import { api, asFailure, unwrap } from '$lib/api/client';
  import { common, stations as copy } from '$lib/copy';
  import { shortAgo } from '$lib/format';
  import { config } from '$lib/live/config.svelte';
  import { saves } from '$lib/live/save.svelte';
  import { stream } from '$lib/sse/stream.svelte';
  import StatusBadge from '$lib/components/StatusBadge.svelte';

  $effect(() => {
    void config.load();
  });

  let busy = $state<string | null>(null);

  /** The component that belongs to a station, for its badge. */
  function componentFor(key: string) {
    return stream.components.find(
      (c) => c.display.section === 'stations' && c.id.endsWith(`:${key}`)
    );
  }

  async function toggle(key: string, collecting: boolean) {
    busy = key;
    try {
      const applied = unwrap(
        await api.PUT('/api/v1/stations/{key}', {
          params: { path: { key } },
          body: { ...toInput(key), collecting }
        })
      );
      saves.accept(applied);
      await config.refresh();
    } catch (e) {
      toast(asFailure(e).summary, 'danger');
    } finally {
      busy = null;
    }
  }

  /**
   * A `PUT` replaces the station, so the toggle has to send the rest of it back. Built
   * from the shared configuration read rather than from a form, which is what stops an
   * inline toggle quietly reverting a field edited in another tab.
   */
  function toInput(key: string) {
    const s = config.station(key);
    if (!s) throw new Error(`no station ${key}`);
    return {
      name: s.name,
      kind: s.kind,
      identity_kind: s.identity_kind,
      collecting: s.collecting,
      latitude: s.latitude,
      longitude: s.longitude,
      altitude_m: s.altitude_m,
      timezone: s.timezone,
      wind_dir_correction_degrees: s.wind_dir_correction_degrees,
      snow_base_distance_mm: s.snow_base_distance_mm,
      poll_interval_seconds: s.poll_interval_seconds,
      aprs_callsign: s.aprs_callsign,
      sensor_mapping: s.sensor_mapping,
      presented_key: s.presented_key,
      transport: {
        kind: s.transport.kind,
        host: s.transport.host,
        port: s.transport.port,
        device: s.transport.device,
        baud: s.transport.baud,
        method: s.transport.method,
        url: s.transport.url,
        topic: s.transport.topic,
        group: s.transport.group,
        mount: s.transport.mount,
        streaming: s.transport.streaming,
        tls_ca_path: s.transport.tls_ca_path,
        tls_cert_path: s.transport.tls_cert_path,
        tls_key_path: s.transport.tls_key_path
      }
    };
  }
</script>

<header class="pagehead">
  <div>
    <h1>{copy.list.title}</h1>
    <p class="blurb">{copy.list.blurb}</p>
  </div>
  <a class="btn btn-primary" href="/stations/new">{copy.list.addButton}</a>
</header>

<Box>
  {#if config.stations.length === 0}
    <EmptyState>
      <h3>{copy.list.empty.heading}</h3>
      <p>{copy.list.empty.body}</p>
      <a class="btn btn-primary" href="/stations/new">{copy.list.empty.action}</a>
    </EmptyState>
  {:else}
    <Table>
      <thead>
        <tr>
          <th>{copy.list.columns.name}</th>
          <th>{copy.list.columns.kind}</th>
          <th>{copy.list.columns.collecting}</th>
          <th>{copy.list.columns.status}</th>
          <th>{copy.list.columns.lastReading}</th>
        </tr>
      </thead>
      <tbody>
        {#each config.stations as station (station.key)}
          {@const component = componentFor(station.key)}
          {@const reading = stream.readings[station.key]}
          {@const received = station.kind === 'grpcreceiver'}
          <tr>
            <td>
              <a href={`/stations/${station.key}`}>{station.name}</a>
              {#if received}
                <Badge variant="info">{copy.list.receivedFrom(station.transport.host ?? 'a peer')}</Badge>
              {/if}
            </td>
            <td>{station.kind_display}</td>
            <td>
              <Toggle
                checked={station.collecting}
                disabled={busy === station.key}
                label={copy.list.columns.collecting}
                onCheckedChange={(next) => toggle(station.key, next)}
              />
            </td>
            <td>
              {#if component}
                <StatusBadge badge={component.badge} state={component.state} live={Boolean(reading)} />
              {:else if received}
                <span class="dim">{copy.list.receivedNote}</span>
              {:else}
                <span class="dim">{common.unknown}</span>
              {/if}
            </td>
            <td>{reading ? shortAgo(reading.ts, stream.now) : '—'}</td>
          </tr>
        {/each}
      </tbody>
    </Table>
  {/if}
</Box>

<style>
  .pagehead {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: 1rem;
    flex-wrap: wrap;
    margin-block-end: 1rem;
  }

  h1 {
    margin: 0 0 0.25rem;
    font-size: var(--text-xl);
  }

  .blurb {
    margin: 0;
    color: var(--color-text-muted);
    font-size: var(--text-sm);
  }

  .dim {
    color: var(--color-text-muted);
    font-size: var(--text-xs);
  }
</style>
