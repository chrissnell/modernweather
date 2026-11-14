<!--
  §7.1: validated **server-side only**. A client-side hint about which of the two fields
  was wrong is a user-enumeration oracle, so there is one message for both cases and this
  page has no client validation beyond "both boxes have something in them".
-->
<script lang="ts">
  import { Box, Button, Input } from '@chrissnell/chonky-ui';
  import { api, asFailure, unwrap, type ApiFailure } from '$lib/api/client';
  import { auth, common } from '$lib/copy';
  import Field from '$lib/components/Field.svelte';
  import { session } from '$lib/live/session.svelte';
  import { goto } from '$app/navigation';

  let username = $state('');
  let password = $state('');
  let busy = $state(false);
  let failure = $state<ApiFailure | null>(null);
  let warnings = $state<string[]>([]);

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    busy = true;
    failure = null;
    try {
      const signedIn = unwrap(
        await api.POST('/api/v1/auth/login', { body: { username, password } })
      );
      warnings = signedIn.warnings;
      await session.load();
      await goto('/', { replaceState: true });
    } catch (e) {
      const f = asFailure(e);
      // 401 and 422 both mean "those two do not match". 429 is the rate limiter, which
      // is worth saying differently because waiting is the fix.
      failure =
        f.status === 429
          ? { ...f, summary: auth.signIn.rateLimited }
          : { ...f, summary: auth.signIn.failed };
    } finally {
      busy = false;
    }
  }
</script>

<div class="wrap">
  <Box title={auth.signIn.title}>
    <form onsubmit={submit}>
      <Field copy={auth.fields.username} required>
        {#snippet children({ id, describedBy })}
          <Input
            {id}
            value={username}
            oninput={(e) => (username = (e.currentTarget as HTMLInputElement).value)}
            aria-describedby={describedBy}
            autocomplete="username"
          />
        {/snippet}
      </Field>

      <Field copy={auth.fields.password} required>
        {#snippet children({ id, describedBy })}
          <Input
            {id}
            type="password"
            value={password}
            oninput={(e) => (password = (e.currentTarget as HTMLInputElement).value)}
            aria-describedby={describedBy}
            autocomplete="current-password"
          />
        {/snippet}
      </Field>

      {#if failure}
        <p class="error" role="alert">{failure.summary}</p>
      {/if}
      {#each warnings as warning (warning)}
        <p class="warning">{warning}</p>
      {/each}

      <Button type="submit" variant="primary" disabled={busy || !username || !password}>
        {busy ? auth.signIn.submitting : common.signIn}
      </Button>
    </form>
  </Box>
</div>

<style>
  .wrap {
    width: min(26rem, 100%);
  }

  .error {
    color: var(--color-danger);
    font-size: var(--text-sm);
  }

  .warning {
    color: var(--color-warning);
    font-size: var(--text-sm);
  }
</style>
