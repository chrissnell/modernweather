<!--
  §7.1's section index: a one-line status per subsection, so the page is a route rather
  than a redirect. "Admin" is deliberately not a top-level item (§0.6) -- it is the thing
  you are looking at -- and its port and certificate live under Advanced.
-->
<script lang="ts">
  import { Badge, Box } from '@chrissnell/chonky-ui';
  import { common, system as copy } from '$lib/copy';
  import { duration, stamp } from '$lib/format';
  import { session } from '$lib/live/session.svelte';
  import { stream } from '$lib/sse/stream.svelte';
  import { api, asFailure, unwrap } from '$lib/api/client';
  import type { Schemas } from '$lib/api/client';

  let info = $state<Schemas['SystemInfoView'] | null>(null);
  let levels = $state<Schemas['LogLevelsView'] | null>(null);

  $effect(() => {
    void (async () => {
      try {
        info = unwrap(await api.GET('/api/v1/system/info'));
      } catch (e) {
        void asFailure(e);
      }
      try {
        levels = unwrap(await api.GET('/api/v1/runtime/log-levels'));
      } catch (e) {
        void asFailure(e);
      }
    })();
  });

  const attention = $derived(
    stream.components.filter((c) => c.state === 'failed' || c.state === 'backoff').length
  );

  function statusFor(href: string): string {
    switch (href) {
      case '/system/activity':
        return `${stream.rows.length} entries buffered.`;
      case '/system/diagnostics':
        return attention === 0
          ? 'Nothing needs attention.'
          : `${attention} ${attention === 1 ? 'thing needs' : 'things need'} attention.`;
      case '/system/machine':
        return info
          ? `Version ${info.version}, running for ${duration(info.uptime_seconds)}.`
          : common.loading;
      case '/system/administrator':
        return session.username ? `Signed in as ${session.username}.` : common.unknown;
      case '/system/advanced':
        return levels ? `Detail: ${levels.directive}` : common.loading;
      default:
        return '';
    }
  }
</script>

<h1>{copy.index.title}</h1>
<p class="blurb">{copy.index.blurb}</p>

{#each copy.index.sections as section (section.href)}
  <Box>
    <div class="row">
      <div>
        <h2><a href={section.href}>{section.label}</a></h2>
        <p class="blurb">{section.summary}</p>
      </div>
      <div class="status">
        {#if section.href === '/system/diagnostics' && attention > 0}
          <Badge variant="danger">{statusFor(section.href)}</Badge>
        {:else}
          <span class="dim">{statusFor(section.href)}</span>
        {/if}
      </div>
    </div>
  </Box>
{/each}

{#if info}
  <p class="dim">Started {stamp(info.started_at)}.</p>
{/if}

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

  .status {
    text-align: right;
  }

  .dim {
    color: var(--color-text-muted);
    font-size: var(--text-xs);
  }
</style>
