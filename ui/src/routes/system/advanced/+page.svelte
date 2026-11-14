<!--
  §7.1's "Advanced", and the **one** screen where §6.6's escape hatch is permitted.

  Every internal term here is written `raw('...')` in `$lib/copy/advanced.ts` and nowhere
  else, so `cargo xtask gate-ui-copy` can print the exception list on every run and the
  list cannot grow quietly. The audience is a person debugging, who needs the internal name
  verbatim because it is what the log and a support message use.

  Two decisions carried from elsewhere and restated where the operator meets them:

  * **A-M7-2-6**: the CORS allowlist is read at startup and no route edits it, so an added
    origin takes effect at the next start. Said on the screen, because a setting that
    silently does nothing until a restart is the defect this whole design exists against.
  * **a port change moves this screen**, so it needs explicit confirmation and the new
    address printed -- a browser does not follow.
-->
<script lang="ts">
  import { Accordion, Box, Button, Input, toast } from '@chrissnell/chonky-ui';
  import { untrack } from 'svelte';
  import { api, asFailure, unwrap, type ApiFailure } from '$lib/api/client';
  import type { Schemas } from '$lib/api/client';
  import { advancedTerms, common, system as copy } from '$lib/copy';
  import Field from '$lib/components/Field.svelte';
  import { config } from '$lib/live/config.svelte';
  import { previewChange, previewSentence, saves } from '$lib/live/save.svelte';
  import { stream } from '$lib/sse/stream.svelte';

  const listener = $derived(config.listener);

  let mgmtAddr = $state('');
  let mgmtPort = $state('');
  let mgmtCert = $state('');
  let mgmtKey = $state('');
  let httpPort = $state('');
  let httpsPort = $state('');
  let listenAddr = $state('');
  let grpcPort = $state('');
  let loadedFor = $state<string | null>(null);

  let busy = $state(false);
  let failure = $state<ApiFailure | null>(null);
  let preview = $state<string | null>(null);
  let levels = $state<Schemas['LogLevelsView'] | null>(null);
  let directive = $state('');

  $effect(() => {
    void config.load();
    void (async () => {
      try {
        levels = unwrap(await api.GET('/api/v1/runtime/log-levels'));
        directive = levels.directive;
      } catch (e) {
        void asFailure(e);
      }
    })();
  });

  $effect(() => {
    const view = listener;
    const stamp = `${config.generation}`;
    if (!view || untrack(() => loadedFor) === stamp) return;
    mgmtAddr = view.management_listen_addr;
    mgmtPort = String(view.management_port);
    mgmtCert = view.management_tls_cert_path ?? '';
    mgmtKey = view.management_tls_key_path ?? '';
    httpPort = String(view.http_port);
    httpsPort = view.https_port ? String(view.https_port) : '';
    listenAddr = view.listen_addr;
    grpcPort = String(view.grpc_port);
    loadedFor = stamp;
  });

  /** Whether saving moves the screen the operator is looking at. */
  const moves = $derived(
    Boolean(
      listener &&
        (mgmtAddr !== listener.management_listen_addr ||
          mgmtPort !== String(listener.management_port))
    )
  );
  const newUrl = $derived(`${mgmtCert ? 'https' : 'http'}://${mgmtAddr}:${mgmtPort}/`);

  function build(): Schemas['ListenerInput'] {
    return {
      listen_addr: listenAddr,
      http_port: Number(httpPort),
      https_port: httpsPort ? Number(httpsPort) : null,
      grpc_port: Number(grpcPort),
      management_listen_addr: mgmtAddr,
      management_port: Number(mgmtPort),
      management_tls_cert_path: mgmtCert || null,
      management_tls_key_path: mgmtKey || null
    };
  }

  async function propose() {
    failure = null;
    preview = null;
    try {
      preview = previewSentence(
        await previewChange({ change: 'put_listener', listener: build() })
      );
    } catch (e) {
      failure = asFailure(e);
    }
  }

  async function save() {
    busy = true;
    failure = null;
    try {
      const applied = unwrap(await api.PUT('/api/v1/website-server', { body: build() }));
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

  async function setDirective() {
    try {
      unwrap(
        await api.POST('/api/v1/runtime/log-level', {
          body: { directive: directive || null }
        })
      );
      levels = unwrap(await api.GET('/api/v1/runtime/log-levels'));
      toast('Applied.', 'success');
    } catch (e) {
      toast(asFailure(e).summary, 'danger');
    }
  }

  function downloadConfig() {
    const text = JSON.stringify(config.value, null, 2);
    const url = URL.createObjectURL(new Blob([text], { type: 'application/json' }));
    const a = document.createElement('a');
    a.href = url;
    a.download = 'configuration.json';
    a.click();
    URL.revokeObjectURL(url);
  }

  const err = (p: string) => failure?.fields.get(p);
</script>

<h1>{copy.advanced.title}</h1>
<p class="blurb">{copy.advanced.blurb}</p>

<Box title={copy.advanced.sections.admin}>
  <Field copy={copy.fields.managementListenAddr} error={err('management_listen_addr')}>
    {#snippet children({ id, describedBy, invalid })}
      <Input
        {id}
        value={mgmtAddr}
        oninput={(e) => (mgmtAddr = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
        aria-invalid={invalid}
      />
    {/snippet}
  </Field>
  <Field copy={copy.fields.managementPort} error={err('management_port')}>
    {#snippet children({ id, describedBy, invalid })}
      <Input
        {id}
        type="number"
        value={mgmtPort}
        oninput={(e) => (mgmtPort = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
        aria-invalid={invalid}
      />
    {/snippet}
  </Field>
  <Field copy={copy.fields.managementTlsCert} error={err('management_tls_cert_path')}>
    {#snippet children({ id, describedBy })}
      <Input
        {id}
        value={mgmtCert}
        oninput={(e) => (mgmtCert = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
        spellcheck={false}
      />
    {/snippet}
  </Field>
  <Field copy={copy.fields.managementTlsKey} error={err('management_tls_key_path')}>
    {#snippet children({ id, describedBy })}
      <Input
        {id}
        value={mgmtKey}
        oninput={(e) => (mgmtKey = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
        spellcheck={false}
      />
    {/snippet}
  </Field>

  {#if listener && listener.management_bound_addr !== listener.management_listen_addr}
    <p class="warn">
      This screen is bound to {listener.management_bound_addr} rather than the configured
      {listener.management_listen_addr}.
    </p>
  {/if}

  <Field copy={copy.fields.listenAddr} error={err('listen_addr')}>
    {#snippet children({ id, describedBy })}
      <Input
        {id}
        value={listenAddr}
        oninput={(e) => (listenAddr = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
      />
    {/snippet}
  </Field>
  <Field copy={copy.fields.httpPort} error={err('http_port')}>
    {#snippet children({ id, describedBy })}
      <Input
        {id}
        type="number"
        value={httpPort}
        oninput={(e) => (httpPort = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
      />
    {/snippet}
  </Field>
  <Field copy={copy.fields.httpsPort} error={err('https_port')}>
    {#snippet children({ id, describedBy })}
      <Input
        {id}
        type="number"
        value={httpsPort}
        oninput={(e) => (httpsPort = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
      />
    {/snippet}
  </Field>
  <Field copy={copy.fields.grpcPort} error={err('grpc_port')}>
    {#snippet children({ id, describedBy })}
      <Input
        {id}
        type="number"
        value={grpcPort}
        oninput={(e) => (grpcPort = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
      />
    {/snippet}
  </Field>

  {#if moves}
    <p class="warn">{copy.advanced.portChangeWarning(newUrl)}</p>
  {/if}
  {#if failure && failure.fields.size === 0}
    <p class="error" role="alert">{failure.summary}</p>
  {/if}

  {#if preview}
    <p class="preview">{preview}</p>
    <div class="actions">
      <Button variant="default" onclick={() => (preview = null)}>{common.cancel}</Button>
      <Button variant={moves ? 'danger' : 'primary'} onclick={save} disabled={busy}>
        {moves ? copy.advanced.portChangeConfirm : common.save}
      </Button>
    </div>
  {:else}
    <div class="actions">
      <Button variant="primary" onclick={propose} disabled={busy}>{common.save}</Button>
    </div>
  {/if}
</Box>

<Box title={copy.advanced.sections.cors}>
  <p class="blurb">{copy.advanced.corsNote}</p>
  <p class="blurb">{copy.advanced.corsEmpty}</p>
</Box>

<Box title={copy.advanced.sections.logLevels}>
  <Field copy={copy.fields.logDirective}>
    {#snippet children({ id, describedBy })}
      <Input
        {id}
        value={directive}
        oninput={(e) => (directive = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
        spellcheck={false}
      />
    {/snippet}
  </Field>
  <div class="actions">
    <Button onclick={setDirective}>{common.save}</Button>
    <Button
      variant="ghost"
      onclick={() => {
        directive = '';
        void setDirective();
      }}
    >
      Back to normal
    </Button>
  </div>
</Box>

<Box title={copy.advanced.sections.ids}>
  <p class="blurb">{copy.advanced.idsNote}</p>
  <p class="blurb">{advancedTerms.internalIdNote}</p>
  <Accordion.Root>
    <Accordion.Item value="ids">
      <Accordion.Trigger>{advancedTerms.internalId}</Accordion.Trigger>
      <Accordion.Content>
        <ul class="ids">
          {#each stream.components as c (c.id)}
            <li>
              <code>{c.id}</code>
              <span class="dim">{c.display.name}</span>
            </li>
          {/each}
        </ul>
      </Accordion.Content>
    </Accordion.Item>
    <Accordion.Item value="vocab">
      <Accordion.Trigger>What the log calls things</Accordion.Trigger>
      <Accordion.Content>
        <p class="blurb">{advancedTerms.stateNote}</p>
        <p class="blurb">{advancedTerms.triggerNote}</p>
        <p class="blurb">{advancedTerms.capabilityNote}</p>
        <p class="blurb">{advancedTerms.spoolNote}</p>
      </Accordion.Content>
    </Accordion.Item>
  </Accordion.Root>
</Box>

<Box title={copy.advanced.sections.raw}>
  <p class="blurb">{copy.advanced.rawNote}</p>
  <Button onclick={downloadConfig} disabled={!config.value}>{common.download}</Button>
</Box>

<Box title={copy.advanced.sections.metrics}>
  <p class="blurb">{copy.advanced.metricsNote}</p>
  <a class="btn btn-sm" href="/metrics" target="_blank" rel="noreferrer">
    {copy.advanced.openMetrics}
  </a>
</Box>

<style>
  h1 {
    margin: 0 0 0.25rem;
    font-size: var(--text-xl);
  }

  .blurb {
    margin: 0 0 0.75rem;
    color: var(--color-text-muted);
    font-size: var(--text-sm);
    max-width: 80ch;
  }

  .warn {
    margin: 0 0 0.75rem;
    color: var(--color-warning);
    font-size: var(--text-sm);
    max-width: 80ch;
  }

  .preview {
    margin: 0 0 0.75rem;
    font-size: var(--text-sm);
  }

  .actions {
    display: flex;
    gap: 0.75rem;
    flex-wrap: wrap;
  }

  .ids {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
    font-size: var(--text-sm);
  }

  .ids li {
    display: flex;
    gap: 0.6rem;
    align-items: baseline;
  }

  .dim {
    color: var(--color-text-muted);
    font-size: var(--text-xs);
  }

  .error {
    color: var(--color-danger);
    font-size: var(--text-sm);
  }
</style>
