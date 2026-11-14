<!--
  §0.7's checklist: "the most important screen in the application", and a local component
  because chonky has no step list (it is `Box` + `Icon check` + `Button` plus this).

  A checklist rather than a modal wizard, because step 3's success criterion is "readings
  are arriving", which can take minutes -- and a wizard that must be finished in one
  sitting is the wrong shape for a step you have to wait for.
-->
<script lang="ts">
  import { Box, Icon } from '@chrissnell/chonky-ui';
  import { auth, common } from '$lib/copy';
  import type { Step } from '$lib/live/firstrun';
  import { complete, current } from '$lib/live/firstrun';

  interface Props {
    steps: Step[];
    /** Set once the operator hides the finished line; the pages stay reachable. */
    dismissed?: boolean;
    ondismiss?: () => void;
  }

  let { steps: list, dismissed = false, ondismiss }: Props = $props();

  const done = $derived(complete(list));
  // **"You are here" only while there is a here.** §0.7's first-run is over when steps 1-3
  // are complete; the optional ones stay available and are "not nagged about". Pointing at
  // one under a heading that says all done is the nagging, and it contradicts itself on
  // the same screen.
  const here = $derived(done ? null : current(list));
</script>

{#if !(done && dismissed)}
  <Box title={done ? auth.firstRun.collapsed : auth.firstRun.heading}>
    {#if done}
      <div class="finished">
        <p>{auth.firstRun.doneWhen}</p>
        {#if ondismiss}
          <button class="btn btn-sm btn-ghost" type="button" onclick={ondismiss}>
            {auth.firstRun.dismiss}
          </button>
        {/if}
      </div>
    {:else}
      <p class="blurb">{auth.setup.blurb}</p>
    {/if}

    <ol>
      {#each list as step, index (step.key)}
        <li class:done={step.done} class:here={step === here}>
          <span class="mark" aria-hidden="true">
            {#if step.done}
              <Icon name="check" size="sm" />
            {:else}
              {index + 1}.
            {/if}
          </span>
          <a href={step.href}>{step.label}</a>
          {#if step.done}
            <span class="tag">{auth.firstRun.done}</span>
          {:else if step.optional}
            <span class="tag">{auth.firstRun.optional}</span>
          {/if}
          {#if step === here}
            <span class="tag here-tag">&larr; {auth.firstRun.youAreHere}</span>
          {/if}
        </li>
      {/each}
    </ol>

    {#if here?.optional}
      <a class="btn btn-sm btn-ghost" href={here.href}>{common.skipForNow}</a>
    {/if}
  </Box>
{/if}

<style>
  .blurb,
  .finished p {
    margin: 0 0 0.75rem;
    color: var(--color-text-muted);
    font-size: var(--text-sm);
  }

  .finished {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    gap: 1rem;
    flex-wrap: wrap;
  }

  .finished p {
    flex: 1 1 24rem;
    margin: 0;
  }

  ol {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }

  li {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    font-size: var(--text-sm);
  }

  .mark {
    display: inline-flex;
    width: 1.5rem;
    justify-content: center;
    color: var(--color-text-muted);
  }

  li.done .mark {
    color: var(--color-success);
  }

  li.done a {
    color: var(--color-text-muted);
  }

  li.here a {
    font-weight: 700;
  }

  .tag {
    font-size: var(--text-xs);
    color: var(--color-text-muted);
  }

  .here-tag {
    color: var(--color-primary);
  }
</style>
