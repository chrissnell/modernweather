<!--
  §0.2 renames "Storage backend" to "Data storage" and narrows it: this is where readings
  are kept and who else gets them, which is three questions and three pages.
-->
<script lang="ts">
  import { Badge, Box } from '@chrissnell/chonky-ui';
  import { common, storage as copy } from '$lib/copy';
  import { config } from '$lib/live/config.svelte';
  import { stream } from '$lib/sse/stream.svelte';
  import StatusBadge from '$lib/components/StatusBadge.svelte';

  $effect(() => {
    void config.load();
  });

  const sections = $derived([
    {
      href: '/data-storage/database',
      label: copy.index.database,
      summary: copy.index.databaseSummary,
      component: stream.components.find((c) => c.id.startsWith('db:') || c.id.startsWith('writer:'))
    },
    {
      href: '/data-storage/send',
      label: copy.index.send,
      summary: copy.index.sendSummary,
      component: stream.components.find((c) => c.id.startsWith('uplink:'))
    },
    {
      href: '/data-storage/receive',
      label: copy.index.receive,
      summary: copy.index.receiveSummary,
      component: stream.components.find((c) => c.id.startsWith('ingest:'))
    }
  ]);
</script>

<h1>{copy.index.title}</h1>
<p class="blurb">{copy.index.blurb}</p>

{#each sections as section (section.href)}
  <Box>
    <div class="row">
      <div>
        <h2><a href={section.href}>{section.label}</a></h2>
        <p class="blurb">{section.summary}</p>
      </div>
      {#if section.component}
        <StatusBadge badge={section.component.badge} state={section.component.state} />
      {:else}
        <Badge variant="default">{common.off}</Badge>
      {/if}
    </div>
  </Box>
{/each}

<style>
  h1 {
    margin: 0 0 0.25rem;
    font-size: var(--text-xl);
  }

  h2 {
    margin: 0;
    font-size: var(--text-base);
  }

  .blurb {
    margin: 0.15rem 0 0;
    color: var(--color-text-muted);
    font-size: var(--text-sm);
  }

  .row {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: 1rem;
    flex-wrap: wrap;
  }
</style>
