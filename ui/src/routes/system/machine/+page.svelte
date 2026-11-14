<!--
  §7.1's "This machine", and §6.3 gap **G12**: there is no description list in chonky, so
  a `Table` with hidden headers is used -- it reads correctly to a screen reader and is
  semantically defensible, which is what the gap note proposes until `DescriptionList`
  exists.
-->
<script lang="ts">
  import { Box, Table } from '@chrissnell/chonky-ui';
  import { api, asFailure, unwrap, type ApiFailure } from '$lib/api/client';
  import type { Schemas } from '$lib/api/client';
  import { common, system as copy } from '$lib/copy';
  import { duration, stamp } from '$lib/format';
  import { config } from '$lib/live/config.svelte';

  let info = $state<Schemas['SystemInfoView'] | null>(null);
  let ports = $state<Schemas['SerialPort'][]>([]);
  let failure = $state<ApiFailure | null>(null);

  $effect(() => {
    void config.load();
    void (async () => {
      try {
        info = unwrap(await api.GET('/api/v1/system/info'));
      } catch (e) {
        failure = asFailure(e);
      }
      try {
        ports = unwrap(await api.GET('/api/v1/system/serial-ports')).ports;
      } catch (e) {
        // A `/dev` that cannot be listed is its own failure and is not the page's: the
        // version and the uptime above are still worth showing.
        failure ??= asFailure(e);
      }
    })();
  });

  const listener = $derived(config.listener);
</script>

<h1>{copy.machine.title}</h1>
<p class="blurb">{copy.machine.blurb}</p>

<Box>
  {#if info}
    <Table>
      <thead class="mw-visually-hidden">
        <tr><th scope="col">What</th><th scope="col">Value</th></tr>
      </thead>
      <tbody>
        <tr><th scope="row">{copy.machine.rows.version}</th><td>{info.version}</td></tr>
        <tr><th scope="row">{copy.machine.rows.target}</th><td>{info.target}</td></tr>
        <tr><th scope="row">{copy.machine.rows.hostname}</th><td>{info.hostname}</td></tr>
        <tr><th scope="row">{copy.machine.rows.os}</th><td>{info.os}</td></tr>
        <tr><th scope="row">{copy.machine.rows.started}</th><td>{stamp(info.started_at)}</td></tr>
        <tr>
          <th scope="row">{copy.machine.rows.uptime}</th>
          <td>{duration(info.uptime_seconds)}</td>
        </tr>
        <tr><th scope="row">{copy.machine.rows.pid}</th><td>{info.pid}</td></tr>
        {#if listener}
          <tr>
            <th scope="row">{copy.machine.rows.listening}</th>
            <td>
              {listener.listen_addr}:{listener.http_port}
              {#if listener.https_port}
                &middot; {listener.listen_addr}:{listener.https_port}
              {/if}
              &middot; {listener.management_bound_addr}:{listener.management_port}
              &middot; {listener.grpc_port}
            </td>
          </tr>
        {/if}
        <tr>
          <th scope="row">{copy.machine.rows.serialPorts}</th>
          <td>
            {#if ports.length === 0}
              <span class="dim">{copy.machine.noSerialPorts}</span>
            {:else}
              <ul>
                {#each ports as port (port.device)}
                  <li>
                    <code>{port.stable_device ?? port.device}</code>
                    {#if port.description}
                      — {port.description}
                    {/if}
                  </li>
                {/each}
              </ul>
            {/if}
          </td>
        </tr>
      </tbody>
    </Table>
  {:else}
    <p class="dim">{common.loading}</p>
  {/if}
  {#if failure}
    <p class="error">{failure.summary}</p>
  {/if}
  <p class="blurb">{copy.machine.unavailable}</p>
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
  }

  ul {
    margin: 0;
    padding-inline-start: 1.1rem;
  }

  .error {
    color: var(--color-danger);
    font-size: var(--text-sm);
  }
</style>
