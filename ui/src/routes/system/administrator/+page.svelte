<!--
  §7.1's "Administrator", and §3.3's answer to the defect it replaces: today's session
  cookie **is** the API credential, so rotating it signs you out of a session you cannot
  otherwise revoke. Here the password and the sessions are separate things, every session
  is individually revocable, and changing the password revokes every other one while
  keeping this one.

  §3.7: there are no API tokens in v1, and the page says so rather than leaving a gap
  where a reader expects one.
-->
<script lang="ts">
  import { AlertDialog, Badge, Box, Button, Input, Table, toast } from '@chrissnell/chonky-ui';
  import { api, asFailure, unwrap, type ApiFailure } from '$lib/api/client';
  import type { Schemas } from '$lib/api/client';
  import { common, system as copy } from '$lib/copy';
  import { ago, stamp } from '$lib/format';
  import Field from '$lib/components/Field.svelte';
  import { session } from '$lib/live/session.svelte';
  import { stream } from '$lib/sse/stream.svelte';

  let sessions = $state<Schemas['SessionView'][]>([]);
  let current = $state('');
  let next = $state('');
  let again = $state('');
  let busy = $state(false);
  let failure = $state<ApiFailure | null>(null);
  let confirmAll = $state(false);

  const minimum = $derived(session.policy?.minimum_characters ?? 12);

  $effect(() => {
    void refresh();
  });

  async function refresh() {
    try {
      sessions = unwrap(await api.GET('/api/v1/auth/sessions')).sessions;
    } catch (e) {
      failure = asFailure(e);
    }
  }

  const mismatch = $derived(again.length > 0 && next !== again);
  const tooShort = $derived(next.length > 0 && next.length < minimum);

  async function change(event: SubmitEvent) {
    event.preventDefault();
    busy = true;
    failure = null;
    try {
      unwrap(
        await api.POST('/api/v1/auth/password', {
          body: { current_password: current, new_password: next, new_password_again: again }
        })
      );
      current = '';
      next = '';
      again = '';
      await refresh();
      toast(copy.administrator.passwordChanged, 'success');
    } catch (e) {
      failure = asFailure(e);
    } finally {
      busy = false;
    }
  }

  async function revoke(id: string) {
    try {
      unwrap(await api.DELETE('/api/v1/auth/sessions/{id}', { params: { path: { id } } }));
      await refresh();
    } catch (e) {
      toast(asFailure(e).summary, 'danger');
    }
  }

  async function revokeAll() {
    confirmAll = false;
    try {
      unwrap(await api.DELETE('/api/v1/auth/sessions'));
      await session.load();
    } catch (e) {
      toast(asFailure(e).summary, 'danger');
    }
  }

  const err = (p: string) => failure?.fields.get(p);
</script>

<h1>{copy.administrator.title}</h1>
<p class="blurb">{copy.administrator.blurb}</p>

<Box title="Who you are">
  <p>
    Signed in as <strong>{session.username ?? common.unknown}</strong>
  </p>
  <p class="blurb">{copy.administrator.recoveryNote}</p>
  <p class="blurb">{copy.administrator.noTokensNote}</p>
</Box>

<Box title={copy.administrator.changePassword}>
  <form onsubmit={change}>
    <Field copy={copy.fields.currentPassword} required error={err('current_password')}>
      {#snippet children({ id, describedBy, invalid })}
        <Input
          {id}
          type="password"
          value={current}
          oninput={(e) => (current = (e.currentTarget as HTMLInputElement).value)}
          aria-describedby={describedBy}
          aria-invalid={invalid}
          autocomplete="current-password"
        />
      {/snippet}
    </Field>
    <Field
      copy={copy.fields.newPassword}
      required
      error={err('new_password') ?? (tooShort ? `At least ${minimum} characters.` : undefined)}
    >
      {#snippet children({ id, describedBy, invalid })}
        <Input
          {id}
          type="password"
          value={next}
          oninput={(e) => (next = (e.currentTarget as HTMLInputElement).value)}
          aria-describedby={describedBy}
          aria-invalid={invalid}
          autocomplete="new-password"
        />
      {/snippet}
    </Field>
    <Field
      copy={copy.fields.newPasswordAgain}
      required
      error={err('new_password_again') ??
        (mismatch ? 'The two passwords are different.' : undefined)}
    >
      {#snippet children({ id, describedBy, invalid })}
        <Input
          {id}
          type="password"
          value={again}
          oninput={(e) => (again = (e.currentTarget as HTMLInputElement).value)}
          aria-describedby={describedBy}
          aria-invalid={invalid}
          autocomplete="new-password"
        />
      {/snippet}
    </Field>
    {#if failure && failure.fields.size === 0}
      <p class="error" role="alert">{failure.summary}</p>
    {/if}
    <Button type="submit" variant="primary" disabled={busy || !current || !next || mismatch}>
      {busy ? common.saving : copy.administrator.changePassword}
    </Button>
  </form>
</Box>

<Box title={copy.administrator.sessionsHeading}>
  {#if sessions.length === 0}
    <p class="blurb">{common.loading}</p>
  {:else}
    <Table>
      <thead>
        <tr>
          <th>{copy.administrator.sessionColumns.created}</th>
          <th>{copy.administrator.sessionColumns.lastSeen}</th>
          <th>{copy.administrator.sessionColumns.from}</th>
          <th>{copy.administrator.sessionColumns.agent}</th>
          <th><span class="mw-visually-hidden">Actions</span></th>
        </tr>
      </thead>
      <tbody>
        {#each sessions as s (s.id)}
          <tr>
            <td>
              {stamp(s.created_at)}
              {#if s.current}
                <Badge variant="info">{copy.administrator.thisSession}</Badge>
              {/if}
            </td>
            <td>{ago(s.last_seen_at, stream.now)}</td>
            <td>{s.created_ip ?? common.unknown}</td>
            <td class="agent">{s.user_agent ?? common.unknown}</td>
            <td>
              {#if !s.current}
                <Button size="sm" onclick={() => revoke(s.id)}>
                  {copy.administrator.revoke}
                </Button>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </Table>
  {/if}

  <AlertDialog.Root bind:open={confirmAll}>
    <AlertDialog.Trigger>
      <Badge variant="danger">{copy.administrator.signOutEverywhere}</Badge>
    </AlertDialog.Trigger>
    <AlertDialog.Content>
      <AlertDialog.Title>{copy.administrator.signOutEverywhereTitle}</AlertDialog.Title>
      <AlertDialog.Description>
        {copy.administrator.signOutEverywhereBody}
      </AlertDialog.Description>
      <AlertDialog.Cancel>{common.cancel}</AlertDialog.Cancel>
      <Button variant="danger" onclick={revokeAll}>
        {copy.administrator.signOutEverywhere}
      </Button>
    </AlertDialog.Content>
  </AlertDialog.Root>
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
    max-width: 78ch;
  }

  .agent {
    max-width: 24ch;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .error {
    color: var(--color-danger);
    font-size: var(--text-sm);
  }
</style>
