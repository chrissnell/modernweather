<!--
  §8.8's offline state, which SvelteKit routes every client-side failure through.
  Overwhelmingly that failure is one thing: the daemon restarted while the browser was
  fetching the next route's chunk, so the fetch was refused.
  **That happens on purpose and routinely.** Adding or removing a station or a website
  restarts every component including the one listener -- M7-1's stated cost -- so the
  moment immediately after such a save is exactly when a navigation can be refused. A page
  that answers it with "500 Internal Error" tells the operator their machine is broken
  when in fact their change is being applied.
-->
<script lang="ts">
  import { Box, Button } from '@chrissnell/chonky-ui';
  import { page } from '$app/state';
  import { common } from '$lib/copy';

  // A refused or aborted fetch is a connection problem; anything else is a real fault and
  // says so rather than being dressed up as one.
  const offline = $derived(
    page.status === 0 ||
      page.status >= 500 ||
      /fetch|load|network|refused/i.test(page.error?.message ?? '')
  );
</script>

<div class="wrap">
  <Box title={offline ? common.offline : 'Something went wrong'}>
    {#if offline}
      <p>{common.offlineHint}</p>
      <p class="dim">
        If you have just saved a change that adds or removes a station or a website, every
        part of RemoteWeather is restarting. That takes a moment and then this page works
        again.
      </p>
    {:else}
      <p class="dim">{page.error?.message ?? 'No further detail.'}</p>
    {/if}
    <div class="actions">
      <Button variant="primary" onclick={() => location.reload()}>{common.retry}</Button>
      <a class="btn" href="/">Home</a>
    </div>
  </Box>
</div>

<style>
  .wrap {
    width: min(34rem, 100%);
    margin-inline: auto;
    margin-block-start: 3rem;
  }

  p {
    margin: 0 0 0.75rem;
    font-size: var(--text-sm);
  }

  .dim {
    color: var(--color-text-muted);
    max-width: 70ch;
  }

  .actions {
    display: flex;
    gap: 0.75rem;
    align-items: center;
  }
</style>
