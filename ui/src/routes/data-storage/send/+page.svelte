<!--
  §7.1's "Send my readings to another RemoteWeather", plus three of **N-11**'s four
  numbers (docs/reconciliation.md C-W7-29) -- the figures W7 specified after W5 had
  finished designing this page:

  * `spool_time_remaining_seconds`;
  * W7 §3.5's `ceiling < max_lag x rate` advisory, which is what makes W7 D-16's ceiling
    policy watchable rather than a coin flip;
  * `peer_reported_depth_envelopes`, **labelled as peer-reported** (W7 D-40) -- shown with
    the peers on `/data-storage/receive`, and the label is on the number here too.

  §8.4's deep-spool state is the one worth reading twice: a backed-up sink is **not** a
  failure, it is a sink doing its job while a peer is away, and the sentence an operator
  needs is how long that can continue.
-->
<script lang="ts">
  import { Badge, Box, EmptyState, Meter } from '@chrissnell/chonky-ui';
  import { common, storage as copy } from '$lib/copy';
  import { bytes, duration, stamp } from '$lib/format';
  import { config } from '$lib/live/config.svelte';
  import { fleet } from '$lib/live/fleet.svelte';
  import { stream } from '$lib/sse/stream.svelte';
  import StatusBadge from '$lib/components/StatusBadge.svelte';

  $effect(() => {
    void config.load();
    void fleet.load();
  });

  const spool = $derived(fleet.spool);
  const uplinks = $derived(stream.components.filter((c) => c.id.startsWith('uplink:')));

  /** N-11: the advisory, rendered from the comparison the server already made. */
  const advisory = $derived.by(() => {
    if (!spool) return null;
    const ceiling = bytes(spool.ceiling_bytes);
    const lag = duration(spool.max_lag_seconds);
    if (spool.ceiling_covers_max_lag === undefined || spool.ceiling_covers_max_lag === null) {
      return null;
    }
    if (spool.ceiling_covers_max_lag) return copy.send.ceilingAdvisoryOk(ceiling, lag);
    return copy.send.ceilingAdvisory(
      ceiling,
      lag,
      bytes(spool.needed_for_max_lag_bytes ?? 0)
    );
  });

  const peerReported = $derived(
    fleet.peers
      .filter(
        (p) =>
          p.peer_reported_depth_envelopes !== undefined &&
          p.peer_reported_depth_envelopes !== null
      )
      .map((p) => ({ name: p.name, depth: p.peer_reported_depth_envelopes as number }))
  );
</script>

<h1>{copy.send.title}</h1>
<p class="blurb">{copy.send.blurb}</p>

<Box title={copy.send.sections.state}>
  {#if uplinks.length === 0}
    <EmptyState>
      <h3>Not sending to anyone</h3>
      <p>Nothing is forwarding readings to another RemoteWeather.</p>
    </EmptyState>
  {:else}
    {#each uplinks as c (c.id)}
      <div class="statusrow">
        <strong>{c.display.qualifier ?? c.display.name}</strong>
        <StatusBadge badge={c.badge} state={c.state} sentence={c.sentence} />
        {#each c.notes as note (note)}
          <p class="blurb">{note}</p>
        {/each}
        {#if c.last_error}
          <pre>{c.last_error}</pre>
        {/if}
      </div>
    {/each}
  {/if}
</Box>

{#if fleet.absent}
  <Box>
    <p class="blurb">
      This machine keeps no unsent readings and has no peers, so there is nothing to
      report here.
    </p>
  </Box>
{:else if spool}
  <Box title={copy.send.unsent}>
    <!-- §8.4: the depth as a Meter against the ceiling, which is the number it is
         actually bounded by -- a bare byte count says nothing about how much room is
         left. -->
    <Meter
      value={spool.depth_bytes}
      min={0}
      max={spool.ceiling_bytes}
      high={Math.round(spool.ceiling_bytes * 0.75)}
      aria-label={copy.send.unsent}
    />
    <p class="figure">
      {#if spool.depth_envelopes !== undefined && spool.depth_envelopes !== null && spool.depth_envelopes > 0}
        {copy.send.unsentWaiting(spool.depth_envelopes, bytes(spool.depth_bytes))}
      {:else if spool.depth_bytes > 0}
        {bytes(spool.depth_bytes)} is waiting to be sent.
      {:else}
        {copy.send.unsentNone}
      {/if}
    </p>

    <!-- N-11: spool_time_remaining_seconds. -->
    <p class="blurb">
      {#if spool.time_remaining_seconds !== undefined && spool.time_remaining_seconds !== null}
        {copy.send.timeRemaining(duration(spool.time_remaining_seconds))}
      {:else}
        {copy.send.timeRemainingUnknown}
      {/if}
    </p>

    <!-- N-11: W7 §3.5's ceiling advisory. -->
    {#if advisory}
      <p
        class="advisory"
        class:warn={spool.ceiling_covers_max_lag === false}
        role={spool.ceiling_covers_max_lag === false ? 'status' : undefined}
      >
        {advisory}
      </p>
    {/if}

    <dl>
      <dt>{copy.send.lastDelivery}</dt>
      <dd>{spool.last_delivery ? stamp(spool.last_delivery) : common.never}</dd>
      <dt>{copy.fields.spoolCeiling.label}</dt>
      <dd>{bytes(spool.ceiling_bytes)}</dd>
      <dt>{copy.fields.spoolMaxLag.label}</dt>
      <dd>{duration(spool.max_lag_seconds)}</dd>
    </dl>
  </Box>

  <!-- N-11: peer-reported depth, labelled as such wherever it appears (W7 D-40). -->
  <Box title={`${copy.send.peerReported} — ${copy.send.unsent.toLowerCase()}`}>
    {#if peerReported.length === 0}
      <p class="blurb">{copy.send.peerReportedNone}</p>
    {:else}
      <ul>
        {#each peerReported as p (p.name)}
          <li>
            <Badge variant="info">{copy.send.peerReported}</Badge>
            <strong>{p.name}</strong>
            {copy.send.peerReportedDepth(p.depth)}
          </li>
        {/each}
      </ul>
    {/if}
    <p class="blurb">{copy.send.peerReportedNote}</p>
  </Box>
{/if}

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

  .figure {
    margin: 0.5rem 0;
    font-size: var(--text-sm);
  }

  .advisory {
    margin: 0 0 0.75rem;
    font-size: var(--text-sm);
    max-width: 78ch;
  }

  .advisory.warn {
    color: var(--color-warning);
  }

  .statusrow {
    margin-block-end: 0.75rem;
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
  }

  dl {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 0.2rem 0.75rem;
    margin: 0;
    font-size: var(--text-sm);
  }

  dt {
    color: var(--color-text-muted);
  }

  dd {
    margin: 0;
  }

  ul {
    list-style: none;
    margin: 0 0 0.75rem;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
    font-size: var(--text-sm);
  }

  li {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    flex-wrap: wrap;
  }

  pre {
    margin: 0;
    padding: 0.5rem;
    overflow-x: auto;
    font-size: var(--text-xs);
    background: var(--color-surface-raised);
    border: 1px solid var(--color-border-subtle);
    border-radius: var(--radius);
  }
</style>
