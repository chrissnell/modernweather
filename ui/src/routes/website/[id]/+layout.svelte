<script lang="ts">
  import { Tabs } from '@chrissnell/chonky-ui';
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import { config } from '$lib/live/config.svelte';
  import { website as copy } from '$lib/copy';
  import type { Snippet } from 'svelte';

  let { children }: { children: Snippet } = $props();

  const id = $derived(Number(page.params.id));
  const site = $derived(config.website(id));

  $effect(() => {
    if (!config.value) void config.load();
  });

  const base = $derived(`/website/${id}`);
  const tab = $derived.by(() => {
    const path = page.url.pathname;
    if (path.endsWith('/features')) return 'features';
    if (path.endsWith('/certificate')) return 'certificate';
    return 'settings';
  });

  function select(next: string) {
    if (next === tab) return;
    void goto(next === 'settings' ? base : `${base}/${next}`);
  }
</script>

<h1>{site?.name ?? `Website ${id}`}</h1>
<p class="blurb">{site?.hostname ?? ''}</p>

<Tabs.Root value={tab} onValueChange={select}>
  <Tabs.List>
    <Tabs.Trigger value="settings">{copy.settings.title}</Tabs.Trigger>
    <Tabs.Trigger value="features">{copy.extras.title}</Tabs.Trigger>
    <Tabs.Trigger value="certificate">{copy.certificate.title}</Tabs.Trigger>
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
