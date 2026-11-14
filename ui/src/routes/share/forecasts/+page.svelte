<!--
  §0.2 names this "Forecasts & alerts" and puts the vendor on the settings page as a
  detail rather than as the feature. §0.4's `Freshness` is rendered here, separately from
  component state, because a forecast that is available but three hours old is not a
  failure -- and the two sentences are different questions.
-->
<script lang="ts">
  import { Box, Button, Checkbox, Input, Toggle, toast } from '@chrissnell/chonky-ui';
  import { api, asFailure, unwrap, type ApiFailure } from '$lib/api/client';
  import type { Schemas } from '$lib/api/client';
  import { common, share as copy } from '$lib/copy';
  import Field from '$lib/components/Field.svelte';
  import StatusBadge from '$lib/components/StatusBadge.svelte';
  import ScheduleLine from '$lib/components/ScheduleLine.svelte';
  import { config } from '$lib/live/config.svelte';
  import { previewChange, previewSentence, saves } from '$lib/live/save.svelte';
  import { stream } from '$lib/sse/stream.svelte';

  const view = $derived(config.forecasts);

  let enabled = $state(false);
  let clientId = $state('');
  let clientSecret = $state('');
  let endpoint = $state('');
  let refresh = $state('');
  let chosen = $state<Record<string, boolean>>({});
  let loadedFor = $state<string | null>(null);

  let busy = $state(false);
  let failure = $state<ApiFailure | null>(null);
  let preview = $state<string | null>(null);

  $effect(() => {
    if (!config.value) void config.load();
  });

  $effect(() => {
    if (!view || loadedFor === `${config.generation}`) return;
    enabled = view.enabled;
    clientId = view.client_id ?? '';
    endpoint = view.api_endpoint ?? '';
    refresh = view.refresh_interval_seconds?.toString() ?? '';
    clientSecret = '';
    chosen = Object.fromEntries(view.stations.map((k) => [k, true]));
    loadedFor = `${config.generation}`;
  });

  const component = $derived(stream.components.find((c) => c.id.startsWith('aeris:')));

  function build(): Schemas['ForecastsInput'] {
    return {
      enabled,
      client_id: clientId === '' ? null : clientId,
      client_secret: clientSecret === '' ? null : clientSecret,
      api_endpoint: endpoint === '' ? null : endpoint,
      refresh_interval_seconds: refresh === '' ? null : Number(refresh),
      stations: Object.entries(chosen)
        .filter(([, on]) => on)
        .map(([k]) => k)
    };
  }

  async function propose() {
    failure = null;
    preview = null;
    try {
      preview = previewSentence(
        await previewChange({ change: 'put_forecasts', forecasts: build() })
      );
    } catch (e) {
      failure = asFailure(e);
    }
  }

  async function save() {
    busy = true;
    failure = null;
    try {
      const applied = unwrap(await api.PUT('/api/v1/forecasts', { body: build() }));
      saves.accept(applied);
      await config.refresh();
      preview = null;
      toast('Saved.', 'success');
    } catch (e) {
      failure = asFailure(e);
    } finally {
      busy = false;
    }
  }

  const err = (p: string) => failure?.fields.get(p);
</script>

<h1>{copy.forecasts.title}</h1>
<p class="blurb">{copy.forecasts.blurb}</p>

<Box title={copy.upload.statusSection}>
  {#if component}
    <StatusBadge badge={component.badge} state={component.state} sentence={component.sentence} />
    <ScheduleLine schedule={component.schedule} now={stream.now} />
    {#each component.notes as note (note)}
      <p class="dim">{note}</p>
    {/each}
    {#if component.last_error}
      <pre>{component.last_error}</pre>
    {/if}
  {:else}
    <p class="dim">Nothing is fetching forecasts yet.</p>
  {/if}
  <p class="dim">{copy.forecasts.credentialTestNote}</p>
</Box>

<Box title="Account">
  <p class="dim">{copy.forecasts.vendorNote}</p>

  <Field copy={copy.fields.enabled}>
    {#snippet children({ id, describedBy })}
      <Toggle
        id={id}
        checked={enabled}
        label={copy.fields.enabled.label}
        aria-describedby={describedBy}
        onCheckedChange={(n) => (enabled = n)}
      />
    {/snippet}
  </Field>

  <Field copy={copy.fields.clientId} error={err('client_id')}>
    {#snippet children({ id, describedBy, invalid })}
      <Input
        {id}
        value={clientId}
        oninput={(e) => (clientId = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
        aria-invalid={invalid}
      />
    {/snippet}
  </Field>

  <Field
    copy={copy.fields.clientSecret}
    note={view?.client_secret.is_set ? 'A secret is stored. Leave empty to keep it.' : undefined}
    error={err('client_secret')}
  >
    {#snippet children({ id, describedBy, invalid })}
      <Input
        {id}
        type="password"
        value={clientSecret}
        oninput={(e) => (clientSecret = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
        aria-invalid={invalid}
        autocomplete="off"
      />
    {/snippet}
  </Field>

  <Field copy={copy.fields.refreshInterval} note="seconds" error={err('refresh_interval_seconds')}>
    {#snippet children({ id, describedBy })}
      <Input
        {id}
        type="number"
        value={refresh}
        oninput={(e) => (refresh = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
      />
    {/snippet}
  </Field>

  <Field copy={copy.fields.apiEndpoint} error={err('api_endpoint')}>
    {#snippet children({ id, describedBy })}
      <Input
        {id}
        value={endpoint}
        oninput={(e) => (endpoint = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
      />
    {/snippet}
  </Field>
</Box>

<Box title="Which stations get a forecast">
  {#if config.stations.length === 0}
    <p class="dim">No stations yet.</p>
  {:else}
    <ul>
      {#each config.stations as s (s.key)}
        <li>
          <Checkbox
            checked={chosen[s.key] ?? false}
            label={s.name}
            onCheckedChange={(n) => (chosen = { ...chosen, [s.key]: n === true })}
          />
        </li>
      {/each}
    </ul>
    <p class="dim">
      A forecast needs the station's coordinates, which are on the station's own settings
      page.
    </p>
  {/if}
</Box>

{#if failure}
  <p class="error" role="alert">{failure.summary}</p>
{/if}

{#if preview}
  <Box>
    <p class="preview">{preview}</p>
    <div class="actions">
      <Button variant="default" onclick={() => (preview = null)}>{common.cancel}</Button>
      <Button variant="primary" onclick={save} disabled={busy}>
        {busy ? common.saving : common.save}
      </Button>
    </div>
  </Box>
{:else}
  <div class="actions">
    <Button variant="primary" onclick={propose} disabled={busy}>{common.save}</Button>
  </div>
{/if}

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
    max-width: 75ch;
  }

  ul {
    list-style: none;
    margin: 0 0 0.75rem;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
  }

  pre {
    margin: 0;
    padding: 0.5rem;
    overflow-x: auto;
    font-size: var(--text-xs);
    background: var(--color-surface-raised);
    border: 1px solid var(--color-border-subtle);
    border-radius: var(--radius);
  }

  .preview {
    margin: 0 0 0.75rem;
    font-size: var(--text-sm);
  }

  .actions {
    display: flex;
    gap: 0.75rem;
    margin-block-start: 1rem;
  }

  .error {
    color: var(--color-danger);
    font-size: var(--text-sm);
  }
</style>
