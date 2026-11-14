<!--
  §2.4's five banner states.

  chonky ships `ApplyBanner { count, onApply }` and this uses it **for ② only**, where a
  count of items catching up is exactly what it shows. The other four states are not
  "apply this" affordances -- ③ is a receipt, ④ is a diagnosis, ⑤ is a decision -- so
  reusing a component named for an Apply button to render them would make the markup lie
  about what the operator can do.
-->
<script lang="ts">
  import { ApplyBanner, Badge, Box, Button, Collapsible, Spinner } from '@chrissnell/chonky-ui';
  import { home } from '$lib/copy';
  import { saves, blockedSentence } from '$lib/live/save.svelte';
  import { toast } from '@chrissnell/chonky-ui';

  let open = $state(false);
  let reverting = $state(false);

  const phase = $derived(saves.phase);

  // ③ holds for three seconds and then goes, per §2.4. An effect rather than a timer in
  // the store, because the fade is a property of this banner being on screen.
  $effect(() => {
    if (phase.kind === 'live') {
      const t = setTimeout(() => saves.clear(), 3_000);
      return () => clearTimeout(t);
    }
  });

  async function undo() {
    reverting = true;
    const failure = await saves.revert();
    reverting = false;
    toast(failure ? home.apply.undoFailed : home.apply.undone, failure ? 'danger' : 'success');
  }
</script>

{#if phase.kind === 'catching_up'}
  <!-- ② JUST SAVED -->
  <ApplyBanner count={phase.remaining} onApply={() => {}} />
  <p class="sr-only" aria-live="polite">{home.apply.savedCatchingUp(phase.remaining)}</p>
{:else if phase.kind === 'live'}
  <!-- ③ LIVE -->
  <Box>
    <p class="row" aria-live="polite">
      <Badge variant="success">{home.apply.savedAndLive}</Badge>
    </p>
  </Box>
{:else if phase.kind === 'not_yet'}
  <!-- ④ STILL NOT LIVE AFTER 10 s -->
  <Box>
    <p class="row" aria-live="polite">
      <Spinner size={14} />
      <strong>{home.apply.savedNotYet}</strong>
      {blockedSentence(phase.blocked)}
      {home.apply.savedNotYetSuffix}
    </p>
    <a class="btn btn-sm btn-ghost" href="/system/diagnostics">{home.apply.whatsWrong}</a>
  </Box>
{:else if phase.kind === 'failed'}
  <!-- ⑤ RESTARTED AND FAILED -->
  <Box>
    <p class="row" role="alert">
      <Badge variant="danger">{home.apply.savedButFailed}</Badge>
      {phase.failed[0]?.last_error ?? phase.failed[0]?.sentence}
    </p>
    <div class="actions">
      <Collapsible.Root bind:open>
        <Collapsible.Trigger>Details</Collapsible.Trigger>
        <Collapsible.Content>
          <ul>
            {#each phase.failed as c (c.id)}
              <li>{c.display.name} — {c.last_error ?? c.sentence}</li>
            {/each}
          </ul>
        </Collapsible.Content>
      </Collapsible.Root>
      {#if !saves.reverted}
        <Button variant="danger" size="sm" onclick={undo} disabled={reverting}>
          {home.apply.undo}
        </Button>
      {/if}
    </div>
  </Box>
{/if}

<style>
  .row {
    margin: 0;
    display: flex;
    align-items: center;
    gap: 0.5rem;
    flex-wrap: wrap;
    font-size: var(--text-sm);
  }

  .actions {
    display: flex;
    gap: 0.75rem;
    align-items: center;
    margin-block-start: 0.5rem;
  }

  ul {
    margin: 0.25rem 0 0;
    padding-inline-start: 1.2rem;
    font-size: var(--text-sm);
  }

  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip: rect(0 0 0 0);
  }
</style>
