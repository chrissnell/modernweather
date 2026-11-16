<!--
  §7.1's "Diagnostics": the checks, and the full component list with run-now and restart.

  **There is no stop button here, on purpose** (Decision 22). `run-now` and `restart` are
  safe and reversible; a general "stop this" creates a running state no configuration
  describes, which is a second source of truth and the thing D-28 exists to remove.
  Turning something off is a configuration change on the page that owns it.

  **No checks section.** W2a §4.4's seven on-demand checks have no components behind them
  -- no work item creates a `check:<name>` -- so the page used to list all seven marked
  "not available". Seven rows that can never do anything are not information; the live
  list below is, and the activity log has the detail.

  The live list is **grouped by `display.section`**, in navigation order. Ungrouped, it is
  one flat run of every component in the process, which is the whole system in arbitrary
  order and reads as noise.

  **One table, not one per group.** A `Table` per group gave each group its own column
  widths, so the Restart buttons -- and every other column -- landed somewhere different
  in each one. The groups are `tbody` elements inside a single table instead, which is
  what makes a column a column.

  **The status cell is the pill and nothing else.** It used to carry §0.4's sentence
  beside the badge, which reads "[Running] Running." on the row where it is least needed
  and is noise on the row where it repeats. The sentence moves under the name, and only
  when it says something the pill does not.
-->
<script lang="ts">
  import { AlertDialog, Badge, Box, Button, Table, toast } from '@chrissnell/chonky-ui';
  import { api, asFailure, unwrap } from '$lib/api/client';
  import { common, nav, system as copy } from '$lib/copy';
  import { stamp } from '$lib/format';
  import StatusBadge from '$lib/components/StatusBadge.svelte';
  import ScheduleLine from '$lib/components/ScheduleLine.svelte';
  import { stream } from '$lib/sse/stream.svelte';

  let busy = $state<string | null>(null);
  // **Two pieces of state, not one.** `AlertDialog.Root` takes a bindable `open` and no
  // change callback (§6.3 gap G16), so dismissing with Escape or the overlay sets `open`
  // to false and nothing else. With a one-way `open={target !== null}` the target stayed
  // set, the next click on the same row changed nothing, and that row's Restart button was
  // dead for the rest of the page's life.
  let restartOpen = $state(false);
  let restartTarget = $state<string | null>(null);

  async function runNow(id: string) {
    busy = id;
    try {
      unwrap(
        await api.POST('/api/v1/runtime/components/{id}/run-now', { params: { path: { id } } })
      );
      toast('Started.', 'success');
    } catch (e) {
      toast(asFailure(e).summary, 'danger');
    } finally {
      busy = null;
    }
  }

  async function restart(id: string) {
    restartOpen = false;
    busy = id;
    try {
      unwrap(
        await api.POST('/api/v1/runtime/components/{id}/restart', { params: { path: { id } } })
      );
      toast('Restarting.', 'success');
    } catch (e) {
      toast(asFailure(e).summary, 'danger');
    } finally {
      busy = null;
    }
  }

  /**
   * The live list, grouped by the part of the UI that owns each component and ordered the
   * way the navigation is. `Section` is a closed enum of exactly the six navigation
   * sections, so every component lands in a group.
   */
  const groups = $derived(
    nav
      .map((item) => ({
        label: item.label,
        items: stream.components.filter((c) => c.display.section === item.section)
      }))
      .filter((group) => group.items.length > 0)
  );

  /** `OnDemand` gets no schedule sentence (§0.4) -- it is a button. */
  function runnable(kind: string): boolean {
    return kind === 'every' || kind === 'calendar' || kind === 'on_demand';
  }

  /**
   * §0.4's sentence, but only when it is not a longer way of saying the badge.
   *
   * "Running." beside a `Running` pill is the case that prompted this; the comparison is
   * on the words rather than a list of known sentences, so a sentence the server grows
   * later -- "Running, but ... isn't available." -- keeps showing up without this needing
   * to be taught about it.
   */
  function detail(sentence: string | undefined, label: string): string | null {
    if (!sentence) return null;
    const bare = sentence
      .replace(/[.…]+$/u, '')
      .trim()
      .toLowerCase();
    return bare === label.trim().toLowerCase() ? null : sentence;
  }
</script>

<h1>{copy.diagnostics.title}</h1>
<p class="blurb">{copy.diagnostics.blurb}</p>

<Box>
  {#if stream.components.length === 0}
    <p class="blurb">{common.loading}</p>
  {:else}
    <div class="scroll">
      <Table>
        <thead>
          <tr>
            <th>{copy.diagnostics.columns.what}</th>
            <th>{copy.diagnostics.columns.status}</th>
            <th>{copy.diagnostics.columns.schedule}</th>
            <th>{copy.diagnostics.columns.lastRun}</th>
            <th class="actions"><span class="mw-visually-hidden">Actions</span></th>
          </tr>
        </thead>
        {#each groups as group (group.label)}
          <tbody>
            <tr>
              <th class="group" colspan="5" scope="colgroup">{group.label}</th>
            </tr>
            {#each group.items as c (c.id)}
              {@const extra = detail(c.sentence, c.badge.label)}
              <tr>
                <td>
                  <span class="what">
                    <a href={c.display.href}>{c.display.name}</a>
                    {#if c.display.qualifier}
                      <span class="dim">{c.display.qualifier}</span>
                    {/if}
                    {#if c.pending}
                      <Badge variant="info">Catching up</Badge>
                    {/if}
                  </span>
                  {#if extra}
                    <p class="note">{extra}</p>
                  {/if}
                  {#each c.notes as note (note)}
                    <p class="note">{note}</p>
                  {/each}
                  {#if c.last_error}
                    <p class="note err">{c.last_error}</p>
                  {/if}
                </td>
                <td><StatusBadge badge={c.badge} state={c.state} /></td>
                <td><ScheduleLine schedule={c.schedule} now={stream.now} /></td>
                <td>{c.schedule.last_run ? stamp(c.schedule.last_run) : common.never}</td>
                <td class="actions">
                  {#if runnable(c.schedule.kind)}
                    <Button size="sm" onclick={() => runNow(c.id)} disabled={busy === c.id}>
                      {copy.diagnostics.runNow}
                    </Button>
                  {/if}
                  <!--
                    One dialog for the table rather than one per row: `AlertDialog.Root`
                    takes a bindable `open` and no change handler, so a per-row instance
                    would need a per-row binding -- and the dialog is modal, so only one can
                    be open anyway.
                  -->
                  <Button
                    size="sm"
                    variant="default"
                    onclick={() => {
                      restartTarget = c.id;
                      restartOpen = true;
                    }}
                    disabled={busy === c.id}
                  >
                    {copy.diagnostics.restart}
                  </Button>
                </td>
              </tr>
            {/each}
          </tbody>
        {/each}
      </Table>
    </div>
  {/if}
</Box>

<p class="blurb">{copy.diagnostics.noStopNote}</p>

<AlertDialog.Root bind:open={restartOpen}>
  <AlertDialog.Content>
    <AlertDialog.Title>{copy.diagnostics.restartTitle}</AlertDialog.Title>
    <AlertDialog.Description>
      {copy.diagnostics.restartBody}
      {#if restartTarget}
        {@const target = stream.components.find((c) => c.id === restartTarget)}
        {#if target}
          <strong>{target.display.name}{target.display.qualifier
              ? ` (${target.display.qualifier})`
              : ''}</strong>
        {/if}
      {/if}
    </AlertDialog.Description>
    <Button variant="default" onclick={() => (restartOpen = false)}>{common.cancel}</Button>
    <Button variant="danger" onclick={() => restartTarget && restart(restartTarget)}>
      {copy.diagnostics.restart}
    </Button>
  </AlertDialog.Content>
</AlertDialog.Root>

<style>
  h1 {
    margin: 0 0 0.5rem;
    font-size: var(--text-xl);
  }

  .blurb {
    margin: 0 0 0.75rem;
    color: var(--color-text-muted);
    font-size: var(--text-sm);
    max-width: 80ch;
  }

  /* The group heading is a row of the one table rather than a title above a table of its
     own, which is what keeps every group's columns in the same place. */
  .group {
    padding-top: 1.1rem;
    color: var(--color-text-muted);
    font-size: var(--text-xs);
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.1em;
    border-bottom: 1px solid var(--color-border);
  }

  tbody:first-of-type .group {
    padding-top: 0;
  }

  .what {
    display: inline-flex;
    align-items: baseline;
    gap: 0.4rem;
    flex-wrap: wrap;
  }

  /* `overflow-wrap` rather than just a max-width: a connection string is one unbreakable
     token, and a table cell is sized by its longest one -- which pushed the whole table
     wider than the panel around it and left the Restart column hanging off the edge. */
  .note {
    margin: 0.2rem 0 0;
    font-size: var(--text-sm);
    color: var(--color-text-muted);
    max-width: 58ch;
    overflow-wrap: anywhere;
  }

  .note.err {
    color: var(--color-danger);
  }

  .dim {
    color: var(--color-text-muted);
    font-size: var(--text-xs);
  }

  .scroll {
    overflow-x: auto;
  }

  /* Proportions rather than content-driven widths. `overflow-wrap: anywhere` above stops
     a connection string forcing the table wider than its panel, but on its own it lets
     the browser shrink the first column to almost nothing and render that error as a
     tall ribbon. These give each column a share of the width and keep the wrapping as a
     safety valve rather than the layout. */
  .scroll :global(thead th:nth-child(1)) {
    width: 38%;
  }

  .scroll :global(thead th:nth-child(2)) {
    width: 14%;
  }

  .scroll :global(thead th:nth-child(3)) {
    width: 20%;
  }

  .scroll :global(thead th:nth-child(4)) {
    width: 14%;
  }
</style>
