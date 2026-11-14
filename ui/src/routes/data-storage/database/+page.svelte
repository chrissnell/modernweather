<!--
  §7.1's "Weather database", and §7.2's last rule is the whole point of it: **a
  connectivity failure is not a validation failure.** Saving a password the database
  rejects is a legitimate save -- the database may be down, or about to be set up -- so the
  save succeeds, the component moves to a failed state, and §2.4's banner reports it.
  Conflating the two is how a UI ends up refusing to let an operator fix a configuration
  while the thing it configures is down.

  §8.3's empty and unreachable states are both here, and both say the thing an operator
  cannot currently determine during an incident: the website and the phone app are served
  from the in-memory snapshot and are unaffected (W3 §2a.6).
-->
<script lang="ts">
  import { Box, Button, EmptyState, Input, Select, toast } from '@chrissnell/chonky-ui';
  import { untrack } from 'svelte';
  import { api, asFailure, unwrap, type ApiFailure } from '$lib/api/client';
  import { common, storage as copy } from '$lib/copy';
  import Field from '$lib/components/Field.svelte';
  import StatusBadge from '$lib/components/StatusBadge.svelte';
  import ScheduleLine from '$lib/components/ScheduleLine.svelte';
  import { config } from '$lib/live/config.svelte';
  import { previewChange, previewSentence, saves } from '$lib/live/save.svelte';
  import { stream } from '$lib/sse/stream.svelte';

  const current = $derived(config.storage);

  let host = $state('');
  let port = $state('5432');
  let database = $state('');
  let user = $state('');
  let password = $state('');
  let sslMode = $state('require');
  let loadedFor = $state<string | null>(null);

  let busy = $state(false);
  let failure = $state<ApiFailure | null>(null);
  let preview = $state<string | null>(null);

  $effect(() => {
    void config.load();
  });

  $effect(() => {
    const view = current;
    const stamp = `${config.generation}`;
    if (!view || untrack(() => loadedFor) === stamp) return;
    host = view.host;
    database = view.database;
    // The connection string is never returned (D-67), so the user and the password are
    // write-only here: an empty box means "leave what is stored alone".
    user = '';
    password = '';
    loadedFor = stamp;
  });

  const components = $derived(
    stream.components.filter(
      (c) => c.id.startsWith('db:') || c.id.startsWith('writer:') || c.id.startsWith('window:')
    )
  );
  const unreachable = $derived(
    components.some((c) => c.state === 'failed' || c.state === 'backoff')
  );

  /**
   * The store takes one connection string rather than six fields. Assembled here, from
   * fields, because "host", "port" and "SSL mode" are what an operator has in front of
   * them -- and a single box that must be in libpq's URL syntax is a transcription error
   * waiting to happen.
   */
  function url(): string {
    const auth = user ? `${encodeURIComponent(user)}${password ? `:${encodeURIComponent(password)}` : ''}@` : '';
    const query = sslMode ? `?sslmode=${encodeURIComponent(sslMode)}` : '';
    return `postgres://${auth}${host}${port ? `:${port}` : ''}/${database}${query}`;
  }

  async function propose() {
    failure = null;
    preview = null;
    try {
      preview = previewSentence(
        await previewChange({ change: 'put_storage', storage: { url: url() } })
      );
    } catch (e) {
      failure = asFailure(e);
    }
  }

  async function save() {
    busy = true;
    failure = null;
    try {
      const applied = unwrap(await api.POST('/api/v1/storage', { body: { url: url() } }));
      saves.accept(applied);
      await config.refresh();
      preview = null;
      password = '';
      toast('Saved.', 'success');
    } catch (e) {
      failure = asFailure(e);
    } finally {
      busy = false;
    }
  }

  const err = (p: string) => failure?.fields.get(p);
</script>

<h1>{copy.database.title}</h1>
<p class="blurb">{copy.database.blurb}</p>

{#if !current}
  <Box>
    <EmptyState>
      <h3>{copy.database.noDatabase.heading}</h3>
      <p>{copy.database.noDatabase.body}</p>
    </EmptyState>
  </Box>
{:else if unreachable}
  <Box title={copy.database.unreachable.heading}>
    <p class="blurb">{copy.database.unreachable.body}</p>
    {#each components as c (c.id)}
      <div class="statusrow">
        <StatusBadge badge={c.badge} state={c.state} sentence={c.sentence} />
      </div>
    {/each}
  </Box>
{/if}

<Box title={copy.database.sections.state}>
  {#if components.length === 0}
    <p class="blurb">Nothing is writing readings yet.</p>
  {:else}
    {#each components as c (c.id)}
      <div class="statusrow">
        <strong>{c.display.name}</strong>
        <StatusBadge badge={c.badge} state={c.state} sentence={c.sentence} />
        <ScheduleLine schedule={c.schedule} now={stream.now} />
        {#each c.notes as note (note)}
          <p class="blurb">{note}</p>
        {/each}
        {#if c.last_error}
          <pre>{c.last_error}</pre>
        {/if}
      </div>
    {/each}
  {/if}
  <p class="blurb">{copy.database.summaryHistoryNote}</p>
  <p class="blurb">{copy.database.unavailable}</p>
</Box>

<Box title={copy.database.sections.connection}>
  <Field copy={copy.fields.host} required error={err('host')}>
    {#snippet children({ id, describedBy, invalid })}
      <Input
        {id}
        value={host}
        oninput={(e) => (host = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
        aria-invalid={invalid}
      />
    {/snippet}
  </Field>
  <Field copy={copy.fields.port} error={err('port')}>
    {#snippet children({ id, describedBy })}
      <Input
        {id}
        type="number"
        value={port}
        oninput={(e) => (port = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
      />
    {/snippet}
  </Field>
  <Field copy={copy.fields.database} required error={err('database')}>
    {#snippet children({ id, describedBy, invalid })}
      <Input
        {id}
        value={database}
        oninput={(e) => (database = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
        aria-invalid={invalid}
      />
    {/snippet}
  </Field>
  <Field
    copy={copy.fields.user}
    note={current?.connection.is_set ? 'A connection is stored. Leave these empty to keep it.' : undefined}
    error={err('user')}
  >
    {#snippet children({ id, describedBy })}
      <Input
        {id}
        value={user}
        oninput={(e) => (user = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
        autocomplete="off"
      />
    {/snippet}
  </Field>
  <Field copy={copy.fields.password} error={err('password')}>
    {#snippet children({ id, describedBy })}
      <Input
        {id}
        type="password"
        value={password}
        oninput={(e) => (password = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
        autocomplete="off"
      />
    {/snippet}
  </Field>
  <Field copy={copy.fields.sslMode}>
    {#snippet children({ id, describedBy })}
      <Select
        id={id}
        aria-describedby={describedBy}
        value={sslMode}
        onValueChange={(v) => (sslMode = v)}
        options={[
          { value: 'require', label: 'require — encrypt the connection' },
          { value: 'prefer', label: 'prefer — encrypt if the server offers it' },
          { value: 'disable', label: 'disable — no encryption (same machine only)' },
          { value: 'verify-full', label: 'verify-full — encrypt and check the certificate' }
        ]}
      />
    {/snippet}
  </Field>

  <p class="blurb">{copy.database.connectivityNote}</p>

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
      <Button variant="primary" onclick={propose} disabled={busy || !host || !database}>
        {common.save}
      </Button>
    </div>
  {/if}
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
    max-width: 75ch;
  }

  .statusrow {
    margin-block-end: 0.75rem;
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
  }

  .error {
    color: var(--color-danger);
    font-size: var(--text-sm);
  }
</style>
