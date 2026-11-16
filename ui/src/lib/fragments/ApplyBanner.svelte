<!--
  §2.4's five banner states.

  chonky ships `ApplyBanner { count, onApply }` and **none of the five uses it**, for the
  reason the name gives away. Its markup is `{count} pending changes` next to a button
  reading `apply`, and §2.1's whole thesis is that there is no staging area and nothing to
  apply: a save *is* the change. ② used to render it with `onApply={() => {}}`, so the
  state an operator sees most often after saving told them their change was **pending**
  and offered them a control that did nothing, while the correct sentence was rendered
  `sr-only` on the next line -- sighted users got the wrong message and screen-reader
  users got the right one. All five states are a `Box` now, and the one sentence serves
  both. (It also puts that copy back where `cargo xtask gate-ui-copy` can see it;
  inside chonky it was beyond the lint's reach.)
-->
<script lang="ts">
  import { Badge, Box, Button, Collapsible, Spinner } from '@chrissnell/chonky-ui';
  import { home } from '$lib/copy';
  import { saves, blockedSentence } from '$lib/live/save.svelte';
  import { toast } from '@chrissnell/chonky-ui';

  let open = $state(false);
  let reverting = $state(false);

  const phase = $derived(saves.phase);

  // ③ holds for three seconds and then goes, per §2.4.
  //
  // **Keyed on the phase's kind, not on the phase.** `saves.phase` is derived from
  // `stream.now`, which ticks every second -- so an effect that read the phase itself was
  // torn down and re-armed before its own three seconds could elapse, and the banner
  // never went away. The one thing this effect may depend on is the string.
  const kind = $derived(phase.kind);
  $effect(() => {
    if (kind !== 'live') return;
    const t = setTimeout(() => saves.clear(), 3_000);
    return () => clearTimeout(t);
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
  <Box>
    <p class="row" aria-live="polite">
      <Spinner size={14} />
      {home.apply.savedCatchingUp(phase.remaining)}
    </p>
  </Box>
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

</style>
