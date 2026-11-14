<!--
  §0.7 step 2. The chooser and the form are `StationForm`, which `/stations/[key]` renders
  too -- they are the same fields over the same resource.
-->
<script lang="ts">
  import { toast } from '@chrissnell/chonky-ui';
  import { goto } from '$app/navigation';
  import { api, asFailure, unwrap, type ApiFailure } from '$lib/api/client';
  import type { Schemas } from '$lib/api/client';
  import { stations as copy } from '$lib/copy';
  import { config } from '$lib/live/config.svelte';
  import { saves } from '$lib/live/save.svelte';
  import StationForm from '$lib/fragments/StationForm.svelte';

  let busy = $state(false);
  let failure = $state<ApiFailure | null>(null);

  async function create(input: Schemas['StationInput']) {
    busy = true;
    failure = null;
    try {
      const applied = unwrap(await api.POST('/api/v1/stations', { body: input }));
      saves.accept(applied);
      await config.refresh();
      toast('Station added.', 'success');
      await goto(`/stations/${input.key ?? ''}/status`);
    } catch (e) {
      failure = asFailure(e);
    } finally {
      busy = false;
    }
  }
</script>

<h1>{copy.add.title}</h1>
<p class="blurb">{copy.add.blurb}</p>

<StationForm {busy} {failure} onsubmit={create} />

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
</style>
