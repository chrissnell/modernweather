<!--
  §7.1's "Station — Settings". The save is §2.4's full sequence: preview on the button,
  commit, then the shell's banner reports the reconciliation.
-->
<script lang="ts">
  import { AlertDialog, Badge, Box, Button, toast } from '@chrissnell/chonky-ui';
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import { api, asFailure, unwrap, type ApiFailure } from '$lib/api/client';
  import type { Schemas } from '$lib/api/client';
  import { common, destinations, stations as copy } from '$lib/copy';
  import { config } from '$lib/live/config.svelte';
  import { previewChange, previewSentence, saves } from '$lib/live/save.svelte';
  import StationForm from '$lib/fragments/StationForm.svelte';

  const key = $derived(page.params.key ?? '');
  const station = $derived(config.station(key));

  let busy = $state(false);
  let failure = $state<ApiFailure | null>(null);
  let pending = $state<Schemas['StationInput'] | null>(null);
  let preview = $state<string | null>(null);
  let previewing = $state(false);
  let confirmDelete = $state(false);
  let deletionConsequence = $state<string | null>(null);

  /**
   * §8.7: the consequences of a deletion come from the **same** `plan()` dry run the save
   * preview uses -- A-M7-1-1's whole point -- so "this also stops your website" is a fact
   * rather than a warning somebody remembered to write.
   */
  $effect(() => {
    if (!confirmDelete) return;
    void (async () => {
      try {
        deletionConsequence = previewSentence(
          await previewChange({ change: 'delete_station', key })
        );
      } catch (e) {
        deletionConsequence = asFailure(e).summary;
      }
    })();
  });

  /**
   * §2.4 ①: the preview runs `plan()` against the proposed configuration and names
   * exactly what the save will restart, before the operator agrees to it.
   */
  async function propose(input: Schemas['StationInput']) {
    pending = input;
    previewing = true;
    failure = null;
    preview = null;
    try {
      preview = previewSentence(
        await previewChange({ change: 'put_station', key, station: input })
      );
    } catch (e) {
      const f = asFailure(e);
      // A 422 from the preview is a validation failure and belongs on the fields; it is
      // not a reason to hide the Save button, because the operator may be mid-edit.
      failure = f;
      if (f.status !== 422) preview = null;
    } finally {
      previewing = false;
    }
  }

  async function save() {
    if (!pending) return;
    busy = true;
    failure = null;
    try {
      const applied = unwrap(
        await api.PUT('/api/v1/stations/{key}', { params: { path: { key } }, body: pending })
      );
      saves.accept(applied);
      await config.refresh();
      pending = null;
      preview = null;
      toast('Saved.', 'success');
    } catch (e) {
      failure = asFailure(e);
    } finally {
      busy = false;
    }
  }

  async function remove() {
    confirmDelete = false;
    try {
      const applied = unwrap(
        await api.DELETE('/api/v1/stations/{key}', { params: { path: { key } } })
      );
      saves.accept(applied);
      await config.refresh();
      await goto('/stations');
    } catch (e) {
      toast(asFailure(e).summary, 'danger');
    }
  }

  const sharedWith = $derived.by(() => {
    const keys = station?.shared_with ?? [];
    return destinations.filter((d) => keys.includes(d.service));
  });
</script>

{#if station}
  <StationForm {station} busy={busy || previewing} {failure} onsubmit={propose} />

  {#if preview && pending}
    <!-- §2.4 ①: the consequence, then the confirmation. -->
    <Box>
      <p class="preview">{preview}</p>
      <div class="actions">
        <Button
          variant="default"
          onclick={() => {
            pending = null;
            preview = null;
          }}
        >
          {common.cancel}
        </Button>
        <Button variant="primary" onclick={save} disabled={busy}>
          {busy ? common.saving : common.save}
        </Button>
      </div>
    </Box>
  {/if}

  <Box title={copy.settings.sections.sharing}>
    {#if sharedWith.length === 0}
      <p class="dim">{copy.settings.sharedWithNone}</p>
    {:else}
      <ul class="shared">
        {#each sharedWith as d (d.service)}
          <li><a href={d.href}>{d.label}</a></li>
        {/each}
      </ul>
    {/if}
  </Box>

  <Box>
    <AlertDialog.Root bind:open={confirmDelete}>
      <AlertDialog.Trigger>
        <Badge variant="danger">{common.remove}</Badge>
      </AlertDialog.Trigger>
      <AlertDialog.Content>
        <AlertDialog.Title>{copy.settings.deleteTitle}</AlertDialog.Title>
        <AlertDialog.Description>{copy.settings.deleteBody}</AlertDialog.Description>
        {#if deletionConsequence}
          <!-- §8.7: a deletion that breaks something else says so first. The sentence
               comes from the same `plan()` dry run the save preview uses. -->
          <p class="consequence">{deletionConsequence}</p>
        {/if}
        <AlertDialog.Cancel>{common.cancel}</AlertDialog.Cancel>
        <!--
          The handler is on a `Button` rather than on `AlertDialog.Action`: the sub-component
          spreads its rest props at runtime but declares only `class` and `children`, so an
          `onclick` on it is a type error (gap **G14**). The dialog's `open` is bound here,
          which is what lets a plain control close it.
        -->
        <Button variant="danger" onclick={remove}>{copy.settings.deleteConfirm}</Button>
      </AlertDialog.Content>
    </AlertDialog.Root>
  </Box>
{:else if config.loading}
  <p class="dim">{common.loading}</p>
{:else}
  <p class="dim">There is no station by that name.</p>
{/if}


<style>
  .preview {
    margin: 0 0 0.75rem;
    font-size: var(--text-sm);
  }

  .actions {
    display: flex;
    gap: 0.75rem;
  }

  .dim {
    color: var(--color-text-muted);
    font-size: var(--text-sm);
  }

  .consequence {
    margin: 0.5rem 0;
    font-size: var(--text-sm);
  }

  .shared {
    margin: 0;
    padding-inline-start: 1.2rem;
    font-size: var(--text-sm);
  }
</style>
