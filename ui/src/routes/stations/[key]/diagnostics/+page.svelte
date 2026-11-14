<!--
  §7.1's "Station — Diagnostics".

  The two actions this page is specified to offer -- Test connection and Read current
  values -- are `POST /api/v1/checks/*`, and the plan is explicit that **no work item
  creates those components**: each check is a `check:<name>` component with
  `Trigger::OnDemand` run through `ComponentControl::run_now`, the routes are two lines
  each and the components are not. Adding them is additive under §4.4 and needs no version
  bump.

  So the page says that, in the operator's words, and points at the two surfaces that do
  work today. A button that silently does nothing would be worse than its absence, and a
  page that pretended the feature did not exist would lose the deep link §2.5 produces.
-->
<script lang="ts">
  import { Box } from '@chrissnell/chonky-ui';
  import { page } from '$app/state';
  import { stations as copy } from '$lib/copy';
  import { stream } from '$lib/sse/stream.svelte';
  import CheckResult from '$lib/components/CheckResult.svelte';

  const key = $derived(page.params.key ?? '');
  const component = $derived(
    stream.components.find((c) => c.display.section === 'stations' && c.id.endsWith(`:${key}`))
  );
</script>

<p class="blurb">{copy.diagnostics.blurb}</p>

<CheckResult
  name={copy.add.testFirst}
  description="Open a connection to the station and report what happened."
  outcome="unavailable"
  message={copy.diagnostics.unavailable}
/>

<CheckResult
  name={copy.diagnostics.readValues}
  description="Ask the station for a reading right now and show the raw values."
  outcome="unavailable"
  message={copy.diagnostics.unavailable}
/>

<Box title="What the running station reports">
  {#if component}
    <p class="sentence">{component.sentence}</p>
    {#if component.last_error}
      <pre>{component.last_error}</pre>
    {/if}
    {#each component.notes as note (note)}
      <p class="blurb">{note}</p>
    {/each}
  {:else}
    <p class="blurb">Nothing is running for this station yet.</p>
  {/if}
  <p>
    <a href={`/system/activity?station=${key}`}>{copy.diagnostics.perStationLog}</a>
  </p>
</Box>

<style>
  .blurb {
    margin: 0 0 0.75rem;
    color: var(--color-text-muted);
    font-size: var(--text-sm);
    max-width: 70ch;
  }

  .sentence {
    margin: 0 0 0.5rem;
    font-size: var(--text-sm);
  }

  pre {
    margin: 0 0 0.5rem;
    padding: 0.5rem;
    overflow-x: auto;
    font-size: var(--text-xs);
    background: var(--color-surface-raised);
    border: 1px solid var(--color-border-subtle);
    border-radius: var(--radius);
  }
</style>
