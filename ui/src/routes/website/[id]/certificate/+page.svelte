<!--
  §7.1's "Website — Security certificate", and §8.5's rule applied as hard as it goes:
  every certificate problem is a **warning on save, never a block**. The operator may be
  pointing at a file that is about to exist, or replacing one that has expired, and
  refusing the save would leave them unable to fix it.

  Reading the certificate back to show its subject and expiry needs a handler that parses
  the PEM, which M7-1's surface does not have. The paths are saved and used; the page says
  what it cannot show rather than printing a plausible blank.
-->
<script lang="ts">
  import { Box, Button, Input, toast } from '@chrissnell/chonky-ui';
  import { page } from '$app/state';
  import { untrack } from 'svelte';
  import { api, asFailure, unwrap, type ApiFailure } from '$lib/api/client';
  import { common, website as copy } from '$lib/copy';
  import Field from '$lib/components/Field.svelte';
  import { config } from '$lib/live/config.svelte';
  import { previewChange, previewSentence, saves } from '$lib/live/save.svelte';

  const id = $derived(Number(page.params.id));
  const site = $derived(config.website(id));

  let certPath = $state('');
  let keyPath = $state('');
  let loadedFor = $state<string | null>(null);
  let busy = $state(false);
  let failure = $state<ApiFailure | null>(null);
  let preview = $state<string | null>(null);

  $effect(() => {
    if (!config.value) void config.load();
  });

  $effect(() => {
    const current = site;
    const key = `${id}:${config.generation}`;
    if (!current || untrack(() => loadedFor) === key) return;
    certPath = current.tls_cert_path ?? '';
    keyPath = current.tls_key_path ?? '';
    loadedFor = key;
  });

  function build() {
    const current = site!;
    return {
      name: current.name,
      hostname: current.hostname,
      page_title: current.page_title,
      about_station_html: current.about_station_html,
      primary_station_key: current.primary_station_key,
      snow_enabled: current.snow_enabled,
      snow_station_key: current.snow_station_key,
      air_quality_enabled: current.air_quality_enabled,
      air_quality_station_key: current.air_quality_station_key,
      apple_app_id: current.apple_app_id,
      is_portal: current.is_portal,
      is_default_tls: current.is_default_tls,
      tls_cert_path: certPath || null,
      tls_key_path: keyPath || null
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
</script>

{#if site}
  <Box title={copy.certificate.title}>
    <p class="blurb">{copy.certificate.blurb}</p>
    {#if !site.tls_cert_path}
      <p class="dim">{copy.certificate.noCertificate}</p>
    {/if}

    <Field copy={copy.fields.tlsCertPath} error={failure?.fields.get('tls_cert_path')}>
      {#snippet children({ id: fid, describedBy, invalid })}
        <Input
          id={fid}
          value={certPath}
          oninput={(e) => (certPath = (e.currentTarget as HTMLInputElement).value)}
          aria-describedby={describedBy}
          aria-invalid={invalid}
          spellcheck={false}
        />
      {/snippet}
    </Field>

    <Field copy={copy.fields.tlsKeyPath} error={failure?.fields.get('tls_key_path')}>
      {#snippet children({ id: fid, describedBy, invalid })}
        <Input
          id={fid}
          value={keyPath}
          oninput={(e) => (keyPath = (e.currentTarget as HTMLInputElement).value)}
          aria-describedby={describedBy}
          aria-invalid={invalid}
          spellcheck={false}
        />
      {/snippet}
    </Field>

    <p class="dim">{copy.certificate.sniNote}</p>
    <p class="dim">{copy.certificate.saveWarningNote}</p>
    <p class="dim">{copy.certificate.unavailable}</p>

    {#if failure && failure.fields.size === 0}
      <p class="error" role="alert">{failure.summary}</p>
    {/if}

    {#if preview}
      <p class="preview">{preview}</p>
      <div class="actions">
        <Button variant="default" onclick={() => (preview = null)}>{common.cancel}</Button>
        <Button variant="primary" onclick={save} disabled={busy}>
          {busy ? common.saving : common.save}
        </Button>
      </div>
    {:else}
      <div class="actions">
        <Button variant="primary" onclick={propose} disabled={busy}>{common.save}</Button>
      </div>
    {/if}
  </Box>
{:else}
  <p class="dim">There is no website with that number.</p>
{/if}

<style>
  .blurb,
  .dim {
    margin: 0 0 0.75rem;
    color: var(--color-text-muted);
    font-size: var(--text-sm);
    max-width: 75ch;
  }

  .preview {
    margin: 0 0 0.75rem;
    font-size: var(--text-sm);
  }

  .actions {
    display: flex;
    gap: 0.75rem;
  }

  .error {
    color: var(--color-danger);
    font-size: var(--text-sm);
  }
</style>
