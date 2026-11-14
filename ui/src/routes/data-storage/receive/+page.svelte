<!--
  §7.1's "Receive readings from another RemoteWeather", plus the half of **N-11** with a
  security argument behind it: **outstanding enrolment tokens and the enrolment log**.

  W7 D-33's reason is the one to keep in view -- *leaving enrolment open is the mistake and
  it should be visible*. `Enroll` is served only while an unexpired, unconsumed token
  exists, so an outstanding row is a door that is open right now, and a page that does not
  say so leaves the operator with no way to find out.

  The log and the outstanding list come from the same rows rather than from a separate
  audit table, which is why they cannot disagree with each other.
-->
<script lang="ts">
  import { Badge, Box, EmptyState, Table } from '@chrissnell/chonky-ui';
  import { common, storage as copy } from '$lib/copy';
  import { ago, stamp } from '$lib/format';
  import { config } from '$lib/live/config.svelte';
  import { fleet } from '$lib/live/fleet.svelte';
  import { stream } from '$lib/sse/stream.svelte';
  import StatusBadge from '$lib/components/StatusBadge.svelte';

  $effect(() => {
    void config.load();
    void fleet.load();
  });

  const ingress = $derived(stream.components.filter((c) => c.id.startsWith('ingest:')));
  const enrolment = $derived(fleet.enrolment);
  const outstanding = $derived(enrolment?.outstanding ?? []);
  const listener = $derived(config.listener);
</script>

<h1>{copy.receive.title}</h1>
<p class="blurb">{copy.receive.blurb}</p>

<Box title={copy.receive.sections.listener}>
  {#if ingress.length === 0}
    <p class="blurb">This machine is not accepting readings from peers.</p>
  {:else}
    {#each ingress as c (c.id)}
      <div class="statusrow">
        <StatusBadge badge={c.badge} state={c.state} sentence={c.sentence} />
        {#if c.last_error}
          <pre>{c.last_error}</pre>
        {/if}
      </div>
    {/each}
  {/if}
  {#if listener}
    <dl>
      <dt>{copy.fields.listenPort.label}</dt>
      <dd>{listener.grpc_port}</dd>
    </dl>
    <p class="blurb">{copy.fields.listenPort.hint}</p>
  {/if}
</Box>

<!-- N-11: enrolment, first, because it is the one with a consequence. -->
<Box title={copy.receive.sections.enrolment}>
  {#if fleet.absent || !enrolment}
    <p class="blurb">
      This machine is not a collector, so nothing can enrol with it.
    </p>
  {:else}
    <p class="advisory" class:open={outstanding.length > 0}>
      {#if outstanding.length > 0}
        <Badge variant="warning">Open</Badge>
        {copy.receive.enrolmentOpen(outstanding.length)}
      {:else}
        <Badge variant="success">Closed</Badge>
        {copy.receive.enrolmentNone}
      {/if}
    </p>

    {#if outstanding.length > 0}
      <Table>
        <thead>
          <tr>
            <th>Peer</th>
            <th>{copy.receive.enrolmentColumns.issued}</th>
            <th>{copy.receive.enrolmentColumns.expires}</th>
          </tr>
        </thead>
        <tbody>
          {#each outstanding as token (token.id)}
            <tr>
              <td>{token.peer_name}</td>
              <td>{stamp(token.issued_at)}</td>
              <td>{ago(token.expires_at, stream.now)}</td>
            </tr>
          {/each}
        </tbody>
      </Table>
    {/if}

    <h3>{copy.receive.enrolmentLog}</h3>
    {#if enrolment.log.length === 0}
      <p class="blurb">{copy.receive.enrolmentLogEmpty}</p>
    {:else}
      <Table>
        <thead>
          <tr>
            <th>{copy.receive.enrolmentLogColumns.when}</th>
            <th>{copy.receive.enrolmentLogColumns.peer}</th>
            <th>{copy.receive.enrolmentLogColumns.outcome}</th>
          </tr>
        </thead>
        <tbody>
          {#each enrolment.log as token (token.id)}
            <tr>
              <td>{stamp(token.issued_at)}</td>
              <td>{token.peer_name}</td>
              <td>
                {#if token.state === 'used'}
                  <Badge variant="success">Enrolled</Badge>
                  {token.consumed_at ? stamp(token.consumed_at) : ''}
                {:else if token.state === 'expired'}
                  <Badge variant="default">Expired unused</Badge>
                {:else}
                  <Badge variant="warning">Still open</Badge>
                {/if}
              </td>
            </tr>
          {/each}
        </tbody>
      </Table>
    {/if}
    <p class="blurb">{copy.receive.unavailable}</p>
  {/if}
</Box>

<Box title={copy.receive.sections.peers}>
  {#if fleet.peers.length === 0}
    <EmptyState>
      <h3>{copy.receive.noPeers.heading}</h3>
      <p>{copy.receive.noPeers.body}</p>
    </EmptyState>
  {:else}
    <Table>
      <thead>
        <tr>
          <th>{copy.receive.peerColumns.name}</th>
          <th>{copy.receive.peerColumns.stations}</th>
          <th>{copy.receive.peerColumns.lastSeen}</th>
          <th>{copy.send.peerReported} {copy.receive.peerColumns.depth.toLowerCase()}</th>
        </tr>
      </thead>
      <tbody>
        {#each fleet.peers as peer (peer.node_id)}
          <tr>
            <td>{peer.name}</td>
            <td>
              {#if peer.stations.length === 0}
                <span class="dim">{common.none}</span>
              {:else}
                {peer.stations
                  .map((k) => config.station(k)?.name ?? k)
                  .join(', ')}
              {/if}
            </td>
            <td>{peer.last_seen ? ago(peer.last_seen, stream.now) : common.never}</td>
            <td>
              {#if peer.peer_reported_depth_envelopes !== undefined && peer.peer_reported_depth_envelopes !== null}
                <Badge variant="info">{copy.send.peerReported}</Badge>
                {peer.peer_reported_depth_envelopes}
              {:else}
                <span class="dim">{copy.send.peerReportedNone}</span>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </Table>
    <p class="blurb">{copy.send.peerReportedNote}</p>
  {/if}
</Box>

<style>
  h1 {
    margin: 0 0 0.25rem;
    font-size: var(--text-xl);
  }

  h3 {
    margin: 1rem 0 0.35rem;
    font-size: var(--text-base);
  }

  .blurb {
    margin: 0 0 0.75rem;
    color: var(--color-text-muted);
    font-size: var(--text-sm);
    max-width: 78ch;
  }

  .advisory {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    flex-wrap: wrap;
    margin: 0 0 0.75rem;
    font-size: var(--text-sm);
  }

  .advisory.open {
    color: var(--color-warning);
  }

  .statusrow {
    margin-block-end: 0.5rem;
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
  }

  dl {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 0.2rem 0.75rem;
    margin: 0 0 0.5rem;
    font-size: var(--text-sm);
  }

  dt {
    color: var(--color-text-muted);
  }

  dd {
    margin: 0;
  }

  .dim {
    color: var(--color-text-muted);
    font-size: var(--text-xs);
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
