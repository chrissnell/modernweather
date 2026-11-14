<!--
  §6.2 maps "Settings / Status / Diagnostics" to `Tabs`, and they are real routes rather
  than tab panes so each one is linkable -- which matters because §2.5's `blocking` list
  and the activity log both link straight to a station's status.
-->
<script lang="ts">
  import { Tabs } from '@chrissnell/chonky-ui';
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import { config } from '$lib/live/config.svelte';
  import { stations as copy } from '$lib/copy';
  import type { Snippet } from 'svelte';

  let { children }: { children: Snippet } = $props();

  const key = $derived(page.params.key ?? '');
  const station = $derived(config.station(key));

  $effect(() => {
    if (!config.value) void config.load();
  });

  const base = $derived(`/stations/${key}`);
  const tab = $derived.by(() => {
    const path = page.url.pathname;
    if (path.endsWith('/status')) return 'status';
    if (path.endsWith('/diagnostics')) return 'diagnostics';
    return 'settings';
  });

  function select(next: string) {
    if (next === tab) return;
    void goto(next === 'settings' ? base : `${base}/${next}`);
  }
</script>

<h1>{station?.name ?? key}</h1>
<p class="blurb">{station?.kind_display ?? ''}</p>

<Tabs.Root value={tab} onValueChange={select}>
  <Tabs.List>
    <Tabs.Trigger value="settings">{copy.settings.title}</Tabs.Trigger>
    <Tabs.Trigger value="status">{copy.status.title}</Tabs.Trigger>
    <Tabs.Trigger value="diagnostics">{copy.diagnostics.title}</Tabs.Trigger>
  </Tabs.List>
</Tabs.Root>

<div class="body">{@render children()}</div>

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

  .body {
    margin-block-start: 1rem;
  }
</style>
