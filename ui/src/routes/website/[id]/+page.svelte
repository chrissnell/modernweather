<!--
  §7.1's "Website — Settings". The hostname is the field `plan()` can reject with
  `RouteConflict { scope, pattern, a, b }` (W3 §6.2), and §7.2 says that error lands **on
  the field**: "Another website already uses weather.example.com." That is only possible
  because `plan()` is pure and runs before anything stops, so the preview behind Save is
  the same call that will do the work.
-->
<script lang="ts">
  import { AlertDialog, Badge, Box, Button, Checkbox, Input, Select, Toggle, toast } from '@chrissnell/chonky-ui';
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import { untrack } from 'svelte';
  import { api, asFailure, unwrap, type ApiFailure } from '$lib/api/client';
  import type { Schemas } from '$lib/api/client';
  import { common, website as copy } from '$lib/copy';
  import Field from '$lib/components/Field.svelte';
  import { config } from '$lib/live/config.svelte';
  import { previewChange, previewSentence, saves } from '$lib/live/save.svelte';

  const id = $derived(Number(page.params.id));
  const site = $derived(config.website(id));

  let name = $state('');
  let hostname = $state('');
  let pageTitle = $state('');
  let aboutHtml = $state('');
  let primaryStation = $state('');
  let snowEnabled = $state(false);
  let snowStation = $state('');
  let airEnabled = $state(false);
  let airStation = $state('');
  let appleAppId = $state('');
  let isPortal = $state(false);
  let isDefaultTls = $state(false);
  let loadedFor = $state<string | null>(null);

  let busy = $state(false);
  let failure = $state<ApiFailure | null>(null);
  let preview = $state<string | null>(null);
  let confirmDelete = $state(false);
  let deletionConsequence = $state<string | null>(null);

  $effect(() => {
    if (!config.value) void config.load();
  });

  // Seeded when the configuration arrives and again after a save, and not in between:
  // a form that re-reads itself mid-edit throws away what the operator typed.
  $effect(() => {
    const current = site;
    const stamp = `${id}:${config.generation}`;
    if (!current || untrack(() => loadedFor) === stamp) return;
    name = current.name;
    hostname = current.hostname;
    pageTitle = current.page_title;
    aboutHtml = current.about_station_html;
    primaryStation = current.primary_station_key ?? '';
    snowEnabled = current.snow_enabled;
    snowStation = current.snow_station_key ?? '';
    airEnabled = current.air_quality_enabled;
    airStation = current.air_quality_station_key ?? '';
    appleAppId = current.apple_app_id;
    isPortal = current.is_portal;
    isDefaultTls = current.is_default_tls;
    loadedFor = stamp;
  });

  $effect(() => {
    if (!confirmDelete) return;
    void (async () => {
      try {
        deletionConsequence = previewSentence(
          await previewChange({ change: 'delete_website', id })
        );
      } catch (e) {
        deletionConsequence = asFailure(e).summary;
      }
    })();
  });

  const stationOptions = $derived([
    { value: '', label: common.none },
    ...config.stations.map((s) => ({ value: s.key, label: s.name }))
  ]);

  function build(): Schemas['WebsiteInput'] {
    return {
      name,
      hostname,
      page_title: pageTitle,
      about_station_html: aboutHtml,
      primary_station_key: primaryStation || null,
      snow_enabled: snowEnabled,
      snow_station_key: snowEnabled ? snowStation || null : null,
      air_quality_enabled: airEnabled,
      air_quality_station_key: airEnabled ? airStation || null : null,
      apple_app_id: appleAppId,
      is_portal: isPortal,
      is_default_tls: isDefaultTls,
      tls_cert_path: site?.tls_cert_path ?? null,
      tls_key_path: site?.tls_key_path ?? null
    };
  }

  async function propose() {
    failure = null;
    preview = null;
    try {
      preview = previewSentence(
        await previewChange({ change: 'put_website', id, website: build() })
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
        await api.PUT('/api/v1/websites/{id}', { params: { path: { id } }, body: build() })
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

  async function remove() {
    confirmDelete = false;
    try {
      const applied = unwrap(
        await api.DELETE('/api/v1/websites/{id}', { params: { path: { id } } })
      );
      saves.accept(applied);
      await config.refresh();
      await goto('/website');
    } catch (e) {
      toast(asFailure(e).summary, 'danger');
    }
  }

  const err = (p: string) => failure?.fields.get(p);
</script>

{#if site}
  <Box title={copy.settings.sections.identity}>
    <Field copy={copy.fields.name} required error={err('name')}>
      {#snippet children({ id: fid, describedBy, invalid })}
        <Input
          id={fid}
          value={name}
          oninput={(e) => (name = (e.currentTarget as HTMLInputElement).value)}
          aria-describedby={describedBy}
          aria-invalid={invalid}
        />
      {/snippet}
    </Field>
    <Field copy={copy.fields.hostname} error={err('hostname')}>
      {#snippet children({ id: fid, describedBy, invalid })}
        <Input
          id={fid}
          value={hostname}
          oninput={(e) => (hostname = (e.currentTarget as HTMLInputElement).value)}
          aria-describedby={describedBy}
          aria-invalid={invalid}
        />
      {/snippet}
    </Field>
    <Field copy={copy.fields.isDefaultTls}>
      {#snippet children({ id: fid, describedBy })}
        <Toggle
          id={fid}
          checked={isDefaultTls}
          label={copy.fields.isDefaultTls.label}
          aria-describedby={describedBy}
          onCheckedChange={(n) => (isDefaultTls = n)}
        />
      {/snippet}
    </Field>
  </Box>

  <Box title={copy.settings.sections.content}>
    <Field copy={copy.fields.pageTitle} error={err('page_title')}>
      {#snippet children({ id: fid, describedBy })}
        <Input
          id={fid}
          value={pageTitle}
          oninput={(e) => (pageTitle = (e.currentTarget as HTMLInputElement).value)}
          aria-describedby={describedBy}
        />
      {/snippet}
    </Field>
    <Field copy={copy.fields.aboutHtml} error={err('about_station_html')}>
      {#snippet children({ id: fid, describedBy })}
        <textarea
          id={fid}
          class="area"
          rows="6"
          value={aboutHtml}
          oninput={(e) => (aboutHtml = (e.currentTarget as HTMLTextAreaElement).value)}
          aria-describedby={describedBy}
        ></textarea>
      {/snippet}
    </Field>
    <Field copy={copy.fields.primaryStation} error={err('primary_station_key')}>
      {#snippet children({ id: fid, describedBy })}
        <Select
          id={fid}
          aria-describedby={describedBy}
          value={primaryStation}
          onValueChange={(v) => (primaryStation = v)}
          options={stationOptions}
        />
      {/snippet}
    </Field>
  </Box>

  <Box title={copy.settings.sections.features}>
    <Field copy={copy.fields.snowEnabled}>
      {#snippet children({ id: fid, describedBy })}
        <Checkbox
          id={fid}
          checked={snowEnabled}
          label={copy.fields.snowEnabled.label}
          aria-describedby={describedBy}
          onCheckedChange={(n) => (snowEnabled = n === true)}
        />
      {/snippet}
    </Field>
    {#if snowEnabled}
      <Field copy={copy.fields.snowStation} error={err('snow_station_key')}>
        {#snippet children({ id: fid, describedBy })}
          <Select
            id={fid}
            aria-describedby={describedBy}
            value={snowStation}
            onValueChange={(v) => (snowStation = v)}
            options={stationOptions}
          />
        {/snippet}
      </Field>
    {/if}
    <Field copy={copy.fields.airQualityEnabled}>
      {#snippet children({ id: fid, describedBy })}
        <Checkbox
          id={fid}
          checked={airEnabled}
          label={copy.fields.airQualityEnabled.label}
          aria-describedby={describedBy}
          onCheckedChange={(n) => (airEnabled = n === true)}
        />
      {/snippet}
    </Field>
    {#if airEnabled}
      <Field copy={copy.fields.airQualityStation} error={err('air_quality_station_key')}>
        {#snippet children({ id: fid, describedBy })}
          <Select
            id={fid}
            aria-describedby={describedBy}
            value={airStation}
            onValueChange={(v) => (airStation = v)}
            options={stationOptions}
          />
        {/snippet}
      </Field>
    {/if}
    <Field copy={copy.fields.isPortal}>
      {#snippet children({ id: fid, describedBy })}
        <Checkbox
          id={fid}
          checked={isPortal}
          label={copy.fields.isPortal.label}
          aria-describedby={describedBy}
          onCheckedChange={(n) => (isPortal = n === true)}
        />
      {/snippet}
    </Field>
  </Box>

  <Box title={copy.settings.sections.app}>
    <Field copy={copy.fields.appleAppId} error={err('apple_app_id')}>
      {#snippet children({ id: fid, describedBy })}
        <Input
          id={fid}
          value={appleAppId}
          oninput={(e) => (appleAppId = (e.currentTarget as HTMLInputElement).value)}
          aria-describedby={describedBy}
        />
      {/snippet}
    </Field>
  </Box>

  {#if failure && failure.fields.size === 0}
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

  <Box>
    <AlertDialog.Root bind:open={confirmDelete}>
      <AlertDialog.Trigger>
        <Badge variant="danger">{common.remove}</Badge>
      </AlertDialog.Trigger>
      <AlertDialog.Content>
        <AlertDialog.Title>{copy.settings.deleteTitle}</AlertDialog.Title>
        <AlertDialog.Description>{copy.settings.deleteBody}</AlertDialog.Description>
        {#if deletionConsequence}
          <p class="preview">{deletionConsequence}</p>
        {/if}
        <AlertDialog.Cancel>{common.cancel}</AlertDialog.Cancel>
        <Button variant="danger" onclick={remove}>{copy.settings.deleteConfirm}</Button>
      </AlertDialog.Content>
    </AlertDialog.Root>
  </Box>
{:else if config.loading}
  <p class="dim">{common.loading}</p>
{:else}
  <p class="dim">There is no website with that number.</p>
{/if}

<style>
  .area {
    width: 100%;
    font-family: var(--font-mono);
    font-size: var(--text-sm);
    background: var(--color-surface-raised);
    color: var(--color-text);
    border: 1px solid var(--color-border);
    border-radius: var(--radius);
    padding: 0.4rem 0.5rem;
  }

  .preview {
    margin: 0 0 0.75rem;
    font-size: var(--text-sm);
  }

  .actions {
    display: flex;
    gap: 0.75rem;
    margin-block: 1rem;
  }

  .dim {
    color: var(--color-text-muted);
    font-size: var(--text-sm);
  }

  .error {
    color: var(--color-danger);
    font-size: var(--text-sm);
  }
</style>
