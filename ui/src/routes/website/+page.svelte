<!--
  §7.1's website list. §0.2 renames the `restserver` controller to "Website & API" and
  this is its one page: "My website".
-->
<script lang="ts">
  import { Badge, Box, EmptyState, Table, toast } from '@chrissnell/chonky-ui';
  import { goto } from '$app/navigation';
  import { api, asFailure, unwrap } from '$lib/api/client';
  import { common, website as copy } from '$lib/copy';
  import { config } from '$lib/live/config.svelte';
  import { saves } from '$lib/live/save.svelte';
  import { stream } from '$lib/sse/stream.svelte';
  import StatusBadge from '$lib/components/StatusBadge.svelte';

  $effect(() => {
    void config.load();
  });

  let busy = $state(false);

  function componentFor(id: number) {
    return stream.components.find(
      (c) => c.display.section === 'website' && c.id.endsWith(`:${id}`)
    );
  }

  /**
   * Creating a site is a `POST` with the minimum a site needs, which lands the operator
   * on its settings page. A separate "Add a website" page would be a form with one field
   * on it, and §7.1 does not have one.
   */
  async function add() {
    busy = true;
    try {
      const applied = unwrap(
        await api.POST('/api/v1/websites', { body: { name: 'My weather website' } })
      );
      saves.accept(applied);
      await config.refresh();
      const created = config.websites[config.websites.length - 1];
      if (created) await goto(`/website/${created.id}`);
    } catch (e) {
      toast(asFailure(e).summary, 'danger');
    } finally {
      busy = false;
    }
  }
</script>

<header class="pagehead">
  <div>
    <h1>{copy.index.title}</h1>
    <p class="blurb">{copy.index.blurb}</p>
  </div>
  <button class="btn btn-primary" type="button" onclick={add} disabled={busy}>
    {copy.index.addButton}
  </button>
</header>

<Box>
  {#if config.websites.length === 0}
    <EmptyState>
      <h3>{copy.index.empty.heading}</h3>
      <p>{copy.index.empty.body}</p>
      <button class="btn btn-primary" type="button" onclick={add} disabled={busy}>
        {copy.index.addButton}
      </button>
    </EmptyState>
  {:else}
    <Table>
      <thead>
        <tr>
          <th>{copy.index.columns.name}</th>
          <th>{copy.index.columns.hostname}</th>
          <th>{copy.index.columns.station}</th>
          <th>{copy.index.columns.status}</th>
          <th>{copy.index.columns.certificate}</th>
        </tr>
      </thead>
      <tbody>
        {#each config.websites as site (site.id)}
          {@const component = componentFor(site.id)}
          <tr>
            <td><a href={`/website/${site.id}`}>{site.name}</a></td>
            <td>{site.hostname || common.none}</td>
            <td>
              {site.primary_station_key
                ? (config.station(site.primary_station_key)?.name ?? site.primary_station_key)
                : common.none}
            </td>
            <td>
              {#if component}
                <StatusBadge badge={component.badge} state={component.state} />
              {:else}
                <Badge variant="default">{common.unknown}</Badge>
              {/if}
            </td>
            <td>
              {#if site.tls_cert_path}
                <a href={`/website/${site.id}/certificate`}>Set</a>
              {:else}
                <span class="dim">{common.none}</span>
              {/if}
            </td>
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
