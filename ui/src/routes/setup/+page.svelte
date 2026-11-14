<!--
  §3.4's one-time claim, and §0.7 step 1 -- the only step that is blocking, because
  nothing else is reachable until the instance has an administrator.

  §6.3 gap **G7**: `PinInput` is a fixed-length short-code control and the setup code is
  52 base32 characters, so this is a plain `Input` that **normalises** -- upper-cases and
  strips the dashes and spaces a person reading a code off a console will type. Evaluated
  `PinInput` with `length=52` as the gap note suggests and it is the wrong control for a
  value that long; not forced.
-->
<script lang="ts">
  import { Box, Button, Input } from '@chrissnell/chonky-ui';
  import { api, asFailure, unwrap, type ApiFailure } from '$lib/api/client';
  import { auth, common } from '$lib/copy';
  import Field from '$lib/components/Field.svelte';
  import { session } from '$lib/live/session.svelte';
  import { goto } from '$app/navigation';

  let code = $state('');
  let username = $state('');
  let password = $state('');
  let again = $state('');
  let busy = $state(false);
  let failure = $state<ApiFailure | null>(null);

  const minimum = $derived(session.policy?.minimum_characters ?? 12);

  /** Dashes, spaces and lower case are all things a person will type. None is an error. */
  function normalise(raw: string): string {
    return raw.replace(/[\s-]+/g, '').toUpperCase();
  }

  const mismatch = $derived(again.length > 0 && password !== again);
  const tooShort = $derived(password.length > 0 && password.length < minimum);

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    busy = true;
    failure = null;
    try {
      unwrap(
        await api.POST('/api/v1/auth/claim', {
          body: {
            setup_code: normalise(code),
            username,
            password,
            password_again: again
          }
        })
      );
      await session.load();
      await goto('/', { replaceState: true });
    } catch (e) {
      failure = asFailure(e);
    } finally {
      busy = false;
    }
  }
</script>

<div class="wrap">
  {#if session.claimed}
    <Box title={auth.setup.claimed.heading}>
      <p>{auth.setup.claimed.body}</p>
      <a class="btn btn-primary" href="/sign-in">{common.signIn}</a>
    </Box>
  {:else}
    <Box title={auth.setup.title}>
      <p class="blurb">{auth.setup.blurb}</p>
      <h2>{auth.setup.step1}</h2>
      <p class="note">{auth.setup.codeNote}</p>

      <form onsubmit={submit}>
        <Field copy={auth.fields.setupCode} required error={failure?.fields.get('setup_code')}>
          {#snippet children({ id, describedBy, invalid })}
            <Input
              {id}
              value={code}
              oninput={(e) => (code = (e.currentTarget as HTMLInputElement).value)}
              aria-describedby={describedBy}
              aria-invalid={invalid}
              autocomplete="off"
              spellcheck={false}
              autocapitalize="characters"
            />
          {/snippet}
        </Field>
        <p class="note">{auth.setup.codeMissing}</p>

        <Field copy={auth.fields.username} required error={failure?.fields.get('username')}>
          {#snippet children({ id, describedBy, invalid })}
            <Input
              {id}
              value={username}
              oninput={(e) => (username = (e.currentTarget as HTMLInputElement).value)}
              aria-describedby={describedBy}
              aria-invalid={invalid}
              autocomplete="username"
            />
          {/snippet}
        </Field>

        <Field
          copy={auth.fields.password}
          required
          error={failure?.fields.get('password') ??
            (tooShort ? `At least ${minimum} characters.` : undefined)}
        >
          {#snippet children({ id, describedBy, invalid })}
            <Input
              {id}
              type="password"
              value={password}
              oninput={(e) => (password = (e.currentTarget as HTMLInputElement).value)}
              aria-describedby={describedBy}
              aria-invalid={invalid}
              autocomplete="new-password"
            />
          {/snippet}
        </Field>

        <Field
          copy={auth.fields.passwordAgain}
          required
          error={failure?.fields.get('password_again') ??
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

        {#if failure}
          <p class="error" role="alert">{failure.summary}</p>
        {/if}

        <Button type="submit" variant="primary" disabled={busy}>
          {busy ? auth.setup.creating : auth.setup.create}
        </Button>
      </form>
    </Box>
  {/if}
</div>

<style>
  .wrap {
    width: min(34rem, 100%);
  }

  h2 {
    margin: 0 0 0.25rem;
    font-size: var(--text-base);
  }

  .blurb,
  .note {
    margin: 0 0 0.75rem;
    color: var(--color-text-muted);
    font-size: var(--text-sm);
  }

  .error {
    color: var(--color-danger);
    font-size: var(--text-sm);
  }
</style>
