<!--
  Weather Underground and PWS Weather. §7.1 describes the second as "Same shape" in as
  many words, and the shape is: one account credential, a per-station identifier, an
  interval, and the status of the thing that does the uploading.

  §7.2's last rule is the one this page exists to honour: **a connectivity failure is not
  a validation failure.** Saving a station ID the service rejects is a legitimate save --
  the service may be down, or the ID may not be registered yet -- so the save succeeds,
  the component moves to a failed state, and the status panel says what the service
  answered. Conflating the two is how a UI ends up refusing to let an operator fix a
  configuration while the thing it configures is down.
-->
<script lang="ts">
  import { Badge, Box, Button, Input, Table, Toggle, toast } from '@chrissnell/chonky-ui';
  import { api, asFailure, unwrap, type ApiFailure } from '$lib/api/client';
  import type { Schemas } from '$lib/api/client';
  import { common, share as copy } from '$lib/copy';
  import { stamp } from '$lib/format';
  import Field from '$lib/components/Field.svelte';
  import StatusBadge from '$lib/components/StatusBadge.svelte';
  import ScheduleLine from '$lib/components/ScheduleLine.svelte';
  import { config } from '$lib/live/config.svelte';
  import { previewChange, previewSentence, saves } from '$lib/live/save.svelte';
  import { stream } from '$lib/sse/stream.svelte';

  interface Props {
    /** The `service` key: `wunderground` or `pwsweather`. */
    service: string;
    title: string;
    blurb: string;
    /** Below this the service is likely to refuse the upload -- a warning, never a block. */
    minimumIntervalSeconds: number;
  }

  let { service, title, blurb, minimumIntervalSeconds }: Props = $props();

  const view = $derived(config.service(service));

  let enabled = $state(false);
  let apiKey = $state('');
  let apiEndpoint = $state('');
  let interval = $state('');
  let pairings = $state<Record<string, string>>({});
  let loadedFor = $state<string | null>(null);

  let busy = $state(false);
  let failure = $state<ApiFailure | null>(null);
  let preview = $state<string | null>(null);

  $effect(() => {
    if (!config.value) void config.load();
  });

  // Reset the form when the configuration arrives or the service changes, and not on
  // every render: a form that re-reads its own fields from the server mid-edit discards
  // what the operator typed.
  $effect(() => {
    if (!view || loadedFor === `${service}:${config.generation}`) return;
    enabled = view.enabled;
    apiEndpoint = view.api_endpoint ?? '';
    interval = view.interval_seconds?.toString() ?? '';
    apiKey = '';
    pairings = Object.fromEntries(
      view.stations.map((p) => [p.station_key, p.remote_station_id ?? ''])
    );
    loadedFor = `${service}:${config.generation}`;
  });

  const component = $derived(
    stream.components.find((c) => c.id.startsWith(`${shortId(service)}:`))
  );

  /** The component id prefix for this service, as `mw-api-mgmt`'s vocabulary assigns it. */
  function shortId(name: string): string {
    return name === 'wunderground' ? 'wu' : name === 'pwsweather' ? 'pws' : name;
  }

  const intervalWarning = $derived.by(() => {
    const n = Number(interval);
    if (!interval || !Number.isFinite(n)) return undefined;
    return n < minimumIntervalSeconds
      ? `This service asks for no more than one upload every ${minimumIntervalSeconds} seconds. It may refuse this.`
      : undefined;
  });

  function build(): Schemas['ServiceInput'] {
    return {
      name: view?.name ?? service,
      enabled,
      // Omitted rather than emptied: no response ever returns a stored credential, so an
      // empty box means "leave it alone", not "clear it".
      api_key: apiKey === '' ? null : apiKey,
      api_endpoint: apiEndpoint === '' ? null : apiEndpoint,
      interval_seconds: interval === '' ? null : Number(interval),
      stations: Object.entries(pairings)
        .filter(([, id]) => id !== '')
        .map(([station_key, remote_station_id]) => ({ station_key, remote_station_id }))
    };
  }

  async function propose() {
    failure = null;
    preview = null;
    try {
      preview = previewSentence(
        await previewChange({ change: 'put_service', service, settings: build() })
      );
    } catch (e) {
      failure = asFailure(e);
    }
  }

  async function save() {
    busy = true;
    failure = null;
    try {
      const applied = unwrap(
        await api.PUT('/api/v1/services/{service}', {
          params: { path: { service } },
          body: build()
        })
      );
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

  const err = (path: string) => failure?.fields.get(path);
</script>

<h1>{title}</h1>
<p class="blurb">{blurb}</p>

<Box title={copy.upload.statusSection}>
  {#if component}
    <StatusBadge
      badge={component.badge}
      state={component.state}
      sentence={component.sentence}
    />
    <ScheduleLine schedule={component.schedule} now={stream.now} />
    {#if component.last_error}
      <p class="label">{copy.upload.lastError}</p>
      <pre>{component.last_error}</pre>
    {/if}
    {#if component.schedule.last_run}
      <p class="dim">{copy.upload.lastUpload}: {stamp(component.schedule.last_run)}</p>
    {/if}
  {:else}
    <Badge variant="default">{common.off}</Badge>
    <p class="dim">Nothing is uploading to this service yet.</p>
  {/if}
  <p class="dim">{copy.upload.rejectedNote}</p>
</Box>

<Box title={copy.upload.globalSection}>
  <Field copy={copy.fields.enabled}>
    {#snippet children({ id, describedBy })}
      <Toggle
        id={id}
        checked={enabled}
        label={copy.fields.enabled.label}
        aria-describedby={describedBy}
        onCheckedChange={(next) => (enabled = next)}
      />
    {/snippet}
  </Field>

  <Field
    copy={copy.fields.apiKey}
    note={view?.api_key.is_set ? 'A password is stored. Leave empty to keep it.' : undefined}
    error={err('api_key')}
  >
    {#snippet children({ id, describedBy, invalid })}
      <Input
        {id}
        type="password"
        value={apiKey}
        oninput={(e) => (apiKey = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
        aria-invalid={invalid}
        autocomplete="off"
      />
    {/snippet}
  </Field>

  <Field copy={copy.fields.interval} note="seconds" error={err('interval_seconds') ?? intervalWarning}>
    {#snippet children({ id, describedBy, invalid })}
      <Input
        {id}
        type="number"
        value={interval}
        oninput={(e) => (interval = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
        aria-invalid={invalid}
      />
    {/snippet}
  </Field>

  <Field copy={copy.fields.apiEndpoint} error={err('api_endpoint')}>
    {#snippet children({ id, describedBy })}
      <Input
        {id}
        value={apiEndpoint}
        oninput={(e) => (apiEndpoint = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
      />
    {/snippet}
  </Field>
</Box>

<Box title={copy.upload.perStationSection}>
  {#if config.stations.length === 0}
    <p class="dim">{copy.upload.noStations}</p>
  {:else}
    <Table>
      <thead>
        <tr><th>Station</th><th>{copy.fields.stationId.label}</th></tr>
      </thead>
      <tbody>
        {#each config.stations as station (station.key)}
          <tr>
            <td>{station.name}</td>
            <td>
              <Input
                value={pairings[station.key] ?? ''}
                aria-label={`${copy.fields.stationId.label} for ${station.name}`}
                oninput={(e) =>
                  (pairings = {
                    ...pairings,
                    [station.key]: (e.currentTarget as HTMLInputElement).value
                  })}
              />
            </td>
          </tr>
        {/each}
      </tbody>
    </Table>
    <p class="dim">{copy.fields.stationId.hint}</p>
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

  .label {
    margin: 0.5rem 0 0.15rem;
    font-size: var(--text-xs);
    color: var(--color-text-muted);
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
