<!--
  §7.1's share grid: one row per destination x station.

  §0.6 puts all four destinations under one heading because "I want my data on Weather
  Underground" is a thing an operator wants, and "controllers" is not. The forecast
  provider is here too -- §0.2 renames it "Forecasts & alerts", with the vendor as a detail
  on its own settings page rather than as the name of the feature.
-->
<script lang="ts">
  import { Badge, Box, EmptyState, Table, Toggle, toast } from '@chrissnell/chonky-ui';
  import { api, asFailure, unwrap } from '$lib/api/client';
  import { common, destinations, share as copy } from '$lib/copy';
  import { stamp } from '$lib/format';
  import { config } from '$lib/live/config.svelte';
  import { saves } from '$lib/live/save.svelte';
  import { stream } from '$lib/sse/stream.svelte';
  import StatusBadge from '$lib/components/StatusBadge.svelte';

  $effect(() => {
    void config.load();
  });

  let busy = $state<string | null>(null);

  function componentFor(service: string) {
    const prefix = service === 'wunderground' ? 'wu' : service === 'pwsweather' ? 'pws' : service;
    return stream.components.filter((c) => c.id.startsWith(`${prefix}:`));
  }

  async function toggle(service: string, enabled: boolean) {
    const view = config.service(service);
    if (!view) return;
    busy = service;
    try {
      const applied = unwrap(
        await api.PUT('/api/v1/services/{service}', {
          params: { path: { service } },
          body: {
            name: view.name,
            enabled,
            api_endpoint: view.api_endpoint,
            interval_seconds: view.interval_seconds,
            aprs_comment: view.aprs_comment,
            aprs_server: view.aprs_server,
            aprs_symbol_code: view.aprs_symbol_code,
            aprs_symbol_table: view.aprs_symbol_table,
            aprs_transport: view.aprs_transport,
            aprs_kiss_destination: view.aprs_kiss_destination,
            aprs_kiss_path: view.aprs_kiss_path,
            aprs_kiss_serial_baud: view.aprs_kiss_serial_baud,
            aprs_kiss_serial_device: view.aprs_kiss_serial_device,
            aprs_kiss_tcp_address: view.aprs_kiss_tcp_address,
            stations: view.stations.map((p) => ({
              station_key: p.station_key,
              remote_station_id: p.remote_station_id
            }))
          }
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
</script>

<h1>{copy.index.title}</h1>
<p class="blurb">{copy.index.blurb}</p>

<Box>
  {#if config.services.length === 0 && !config.forecasts}
    <EmptyState>
      <h3>{copy.index.empty.heading}</h3>
      <p>{copy.index.empty.body}</p>
    </EmptyState>
  {/if}
  <Table>
    <thead>
      <tr>
        <th>{copy.index.columns.destination}</th>
        <th>{copy.index.columns.station}</th>
        <th>{copy.index.columns.enabled}</th>
        <th>{copy.index.columns.status}</th>
        <th>{copy.index.columns.lastSent}</th>
      </tr>
    </thead>
    <tbody>
      {#each destinations as d (d.service)}
        {@const view = config.service(d.service)}
        {@const parts = componentFor(d.service)}
        <tr>
          <td><a href={d.href}>{d.label}</a></td>
          <td>
            {#if view && view.stations.length > 0}
              {view.stations.map((p) => config.station(p.station_key)?.name ?? p.station_key).join(', ')}
            {:else}
              <span class="dim">{common.none}</span>
            {/if}
          </td>
          <td>
            {#if view}
              <Toggle
                checked={view.enabled}
                disabled={busy === d.service}
                label={copy.index.columns.enabled}
                onCheckedChange={(next) => toggle(d.service, next)}
              />
            {:else}
              <span class="dim">{common.notAvailableYet}</span>
            {/if}
          </td>
          <td>
            {#if parts.length > 0}
              {#each parts as c (c.id)}
                <StatusBadge badge={c.badge} state={c.state} />
              {/each}
            {:else}
              <Badge variant="default">{common.off}</Badge>
            {/if}
          </td>
          <td>
            {#if parts.length > 0 && parts[0].schedule.last_run}
              {stamp(parts[0].schedule.last_run)}
            {:else}
              <span class="dim">{common.never}</span>
            {/if}
          </td>
        </tr>
      {/each}
      <tr>
        <td><a href={copy.index.forecastsHref}>{copy.index.forecastsLabel}</a></td>
        <td>
          {#if config.forecasts && config.forecasts.stations.length > 0}
            {config.forecasts.stations
              .map((k) => config.station(k)?.name ?? k)
              .join(', ')}
          {:else}
            <span class="dim">{common.none}</span>
          {/if}
        </td>
        <td>
          {#if config.forecasts}
            <Badge variant={config.forecasts.enabled ? 'success' : 'default'}>
              {config.forecasts.enabled ? common.on : common.off}
            </Badge>
          {:else}
            <span class="dim">{common.off}</span>
          {/if}
        </td>
        <td>
          {#each stream.components.filter((c) => c.id.startsWith('aeris:')) as c (c.id)}
            <StatusBadge badge={c.badge} state={c.state} />
          {/each}
        </td>
        <td class="dim">{copy.index.forecastsSummary}</td>
      </tr>
    </tbody>
  </Table>
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

  .dim {
    color: var(--color-text-muted);
    font-size: var(--text-xs);
  }
</style>
