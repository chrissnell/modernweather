<!--
  The shell, the route guard and the one live connection's lifecycle.

  Three things happen here and nowhere else, each because doing it per page would mean
  doing it twenty-six times and getting it wrong once:

  * **the guard.** An unclaimed instance can only reach `/setup` and an unauthenticated
    one only `/sign-in` (§7.1). Decided before the application renders, so there is no
    flash of an admin screen behind a sign-in form (§8.8).
  * **the stream.** One `EventSource` for the whole application (§5.1), opened once here.
  * **the banner.** §2.4's reconciliation report follows the operator off the page they
    saved on, because the thing it reports on takes seconds and they will have navigated.
-->
<script lang="ts">
  import '../app.css';
  import { Box, Spinner } from '@chrissnell/chonky-ui';
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import AppShell from '$lib/components/AppShell.svelte';
  import ApplyBannerReport from '$lib/fragments/ApplyBanner.svelte';
  import { session } from '$lib/live/session.svelte';
  import { stream } from '$lib/sse/stream.svelte';
  import { auth, common } from '$lib/copy';
  import type { Snippet } from 'svelte';

  let { children }: { children: Snippet } = $props();

  const UNGUARDED = ['/setup', '/sign-in'];

  let ready = $state(false);

  $effect(() => {
    session.load().then(() => (ready = true));
    return () => stream.stop();
  });

  // The guard, as an effect on the session rather than a `load` function: SPA mode has no
  // server to redirect from, and the answer changes while the application is open (a
  // session revoked from another browser must land here within one request).
  $effect(() => {
    if (!ready) return;
    const path = page.url.pathname;
    if (!session.claimed && path !== '/setup') {
      goto('/setup', { replaceState: true });
      return;
    }
    if (session.claimed && !session.authenticated && !UNGUARDED.includes(path)) {
      goto('/sign-in', { replaceState: true });
      return;
    }
    if (session.claimed && session.authenticated && UNGUARDED.includes(path)) {
      goto('/', { replaceState: true });
    }
  });

  // The stream is only useful once there is a session to authorise it, and §5.6's channel
  // set depends on the page: `components` everywhere, `readings` where a reading is shown,
  // `logs` only on the activity view -- which is what keeps the log channel's volume off
  // every other page's connection.
  $effect(() => {
    if (!session.authenticated) return;
    const path = page.url.pathname;
    const channels: ('components' | 'logs' | 'readings')[] = ['components'];
    if (path === '/' || path.startsWith('/stations')) channels.push('readings');
    if (path === '/system/activity') channels.push('logs');
    if (stream.state.phase === 'connecting' && !stream.streamId) stream.start(channels);
    else void stream.setChannels(channels);
  });

  const guarded = $derived(!UNGUARDED.includes(page.url.pathname));
</script>

{#if !ready}
  <div class="boot">
    <Spinner size={20} />
    <span>{common.loading}</span>
  </div>
{:else if session.error}
  <div class="boot">
    <Box title={common.offline}>
      <p>{common.offlineHint}</p>
      <p class="detail">{session.error.summary}</p>
    </Box>
  </div>
{:else if !guarded}
  <div class="plain">
    {@render children()}
  </div>
{:else if session.authenticated}
  <AppShell>
    {#snippet banner()}
      <ApplyBannerReport />
    {/snippet}
    {@render children()}
  </AppShell>
{:else}
  <div class="boot">
    <Spinner size={20} />
    <span>{auth.signIn.sessionExpired}</span>
  </div>
{/if}

<style>
  .boot {
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 0.75rem;
    min-height: 60vh;
    color: var(--color-text-muted);
    padding: 1rem;
  }

  .plain {
    display: flex;
    align-items: center;
    justify-content: center;
    min-height: 100vh;
    padding: 1rem;
  }

  .detail {
    color: var(--color-text-muted);
    font-size: var(--text-sm);
  }
</style>
