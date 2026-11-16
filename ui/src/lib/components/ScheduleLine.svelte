<!--
  §0.4's trigger table, rendered.

  The sentence itself comes from the server (§2.3: "the server renders the vocabulary, not
  the client"), so what is left here is the three facts around it -- last run, its outcome,
  and when the next one is due -- which W2a §1.3 makes real state rather than a guess,
  because the clock lives in the supervisor instead of inside a `select!` loop.

  `OnDemand` gets no sentence at all, per §0.4: it is a button, not a schedule.
-->
<script lang="ts">
  import { ago, stamp } from '$lib/format';
  import type { Schemas } from '$lib/api/client';

  interface Props {
    schedule: Schemas['ScheduleView'];
    now?: number;
  }

  let { schedule, now = Date.now() }: Props = $props();

  const outcome = $derived.by(() => {
    switch (schedule.last_outcome) {
      case 'ok':
        return 'succeeded';
      case 'failed':
        return 'failed';
      case 'skipped':
        return 'was skipped because the previous one was still going';
      case 'deadline_exceeded':
        return 'gave up before it finished';
      default:
        return null;
    }
  });
</script>

{#if schedule.sentence}
  <p class="line">{schedule.sentence}</p>
{/if}
{#if schedule.last_run}
  <p class="line dim">
    Last run {ago(schedule.last_run, now)}{#if outcome}, and it {outcome}{/if}.
  </p>
{/if}
{#if schedule.next_run}
  <p class="line dim">Next run {stamp(schedule.next_run)}.</p>
{/if}

<style>
  .line {
    margin: 0;
    font-size: var(--text-sm);
  }

  .dim {
    color: var(--color-text-muted);
  }
</style>
