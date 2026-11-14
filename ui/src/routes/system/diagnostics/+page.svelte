<!--
  §7.1's "Diagnostics": the checks, and the full component list with run-now and restart.

  **There is no stop button here, on purpose** (Decision 22). `run-now` and `restart` are
  safe and reversible; a general "stop this" creates a running state no configuration
  describes, which is a second source of truth and the thing D-28 exists to remove.
  Turning something off is a configuration change on the page that owns it.

  The seven checks are `POST /api/v1/checks/*`, and the plan is explicit that **no work
  item creates those components** -- each is a `check:<name>` with `Trigger::OnDemand`, the
  routes are two lines each and the components are not. So they are listed with what they
  would do and marked unavailable, rather than being seven buttons that answer
  `NoSuchComponent`.
-->
<script lang="ts">
  import { AlertDialog, Badge, Box, Button, Table, toast } from '@chrissnell/chonky-ui';
  import { api, asFailure, unwrap } from '$lib/api/client';
  import { common, system as copy } from '$lib/copy';
  import { stamp } from '$lib/format';
  import CheckResult from '$lib/components/CheckResult.svelte';
  import StatusBadge from '$lib/components/StatusBadge.svelte';
  import ScheduleLine from '$lib/components/ScheduleLine.svelte';
  import { stream } from '$lib/sse/stream.svelte';

  /** W2a §4.4's seven, named in the operator's words. */
  const CHECKS = [
    { name: 'Weather station', description: 'Open a connection to each station and read a value.' },
    { name: 'Weather database', description: 'Connect, and check the schema is the one this build expects.' },
    { name: 'Weather Underground', description: 'Send a test upload and report what they answered.' },
    { name: 'PWS Weather', description: 'Send a test upload and report what they answered.' },
    { name: 'Forecasts & alerts', description: 'Fetch a forecast with the stored account details.' },
    { name: 'Another RemoteWeather', description: 'Open the peer link and confirm the handshake.' },
    {
      name: 'APRS / CWOP',
      description:
        'Check the beacon path. This is the connectivity check the 90-second health monitor was already doing.'
    }
  ];

  let busy = $state<string | null>(null);
  let confirmRestart = $state<string | null>(null);

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
    confirmRestart = null;
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

  /** `OnDemand` gets no schedule sentence (§0.4) -- it is a button. */
  function runnable(kind: string): boolean {
    return kind === 'every' || kind === 'calendar' || kind === 'on_demand';
  }
</script>

<h1>{copy.diagnostics.title}</h1>
<p class="blurb">{copy.diagnostics.blurb}</p>

<h2>{copy.diagnostics.checksHeading}</h2>
<p class="blurb">{copy.diagnostics.checksUnavailable}</p>
{#each CHECKS as check (check.name)}
  <CheckResult
    name={check.name}
    description={check.description}
    outcome="unavailable"
    message={copy.diagnostics.checksUnavailable}
  />
{/each}

<h2>{copy.diagnostics.runningHeading}</h2>
<Box>
  {#if stream.components.length === 0}
    <p class="blurb">{common.loading}</p>
  {:else}
    <Table>
      <thead>
        <tr>
          <th>{copy.diagnostics.columns.what}</th>
          <th>{copy.diagnostics.columns.status}</th>
          <th>{copy.diagnostics.columns.schedule}</th>
          <th>{copy.diagnostics.columns.lastRun}</th>
          <th><span class="mw-visually-hidden">Actions</span></th>
        </tr>
      </thead>
      <tbody>
        {#each stream.components as c (c.id)}
          <tr>
            <td>
              <a href={c.display.href}>{c.display.name}</a>
              {#if c.display.qualifier}
                <span class="dim">{c.display.qualifier}</span>
              {/if}
              {#if c.pending}
                <Badge variant="info">Catching up</Badge>
              {/if}
            </td>
            <td>
              <StatusBadge badge={c.badge} state={c.state} sentence={c.sentence} />
              {#each c.notes as note (note)}
                <p class="note">{note}</p>
              {/each}
              {#if c.last_error}
                <p class="note err">{c.last_error}</p>
              {/if}
            </td>
            <td><ScheduleLine schedule={c.schedule} now={stream.now} /></td>
            <td>{c.schedule.last_run ? stamp(c.schedule.last_run) : common.never}</td>
            <td class="actions">
              {#if runnable(c.schedule.kind)}
                <Button size="sm" onclick={() => runNow(c.id)} disabled={busy === c.id}>
                  {copy.diagnostics.runNow}
                </Button>
              {/if}
              <!--
                One dialog for the table rather than one per row: `AlertDialog.Root` takes
                a bindable `open` and no change handler, so a per-row instance would need a
                per-row binding -- and the dialog is modal, so only one can be open anyway.
              -->
              <Button
                size="sm"
                variant="default"
                onclick={() => (confirmRestart = c.id)}
                disabled={busy === c.id}
              >
                {copy.diagnostics.restart}
              </Button>
            </td>
          </tr>
        {/each}
      </tbody>
    </Table>
  {/if}
  <p class="blurb">{copy.diagnostics.noStopNote}</p>
</Box>

<AlertDialog.Root open={confirmRestart !== null}>
  <AlertDialog.Content>
    <AlertDialog.Title>{copy.diagnostics.restartTitle}</AlertDialog.Title>
    <AlertDialog.Description>{copy.diagnostics.restartBody}</AlertDialog.Description>
    <Button variant="default" onclick={() => (confirmRestart = null)}>{common.cancel}</Button>
    <Button
      variant="danger"
      onclick={() => confirmRestart && restart(confirmRestart)}
    >
      {copy.diagnostics.restart}
    </Button>
  </AlertDialog.Content>
</AlertDialog.Root>

<style>
  h1 {
    margin: 0 0 0.25rem;
    font-size: var(--text-xl);
  }

  h2 {
    margin: 1.25rem 0 0.35rem;
    font-size: var(--text-lg);
  }

  .blurb {
    margin: 0 0 0.75rem;
    color: var(--color-text-muted);
    font-size: var(--text-sm);
    max-width: 80ch;
  }

  .note {
    margin: 0.2rem 0 0;
    font-size: var(--text-xs);
    color: var(--color-text-muted);
  }

  .note.err {
    color: var(--color-danger);
  }

  .dim {
    color: var(--color-text-muted);
    font-size: var(--text-xs);
  }

  .actions {
    display: flex;
    gap: 0.4rem;
    align-items: center;
  }
</style>
