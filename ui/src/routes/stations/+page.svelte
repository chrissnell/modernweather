<!--
  §7.1's station list. The one editable thing here is the inline "Collecting readings"
  toggle, which is a configuration change like any other: it is saved, previewed and
  reported on, which is what makes it survive a restart (Decision 22).

  §0.6: stations received from a peer appear here too, badged, and read-only -- this is
  where an operator looks for "what stations do I have", and the editing lives where the
  pairing is configured.
-->
<script lang="ts">
  import { AlertDialog, Badge, Box, Button, EmptyState, Table, Toggle, toast } from '@chrissnell/chonky-ui';
  import { api, asFailure, unwrap } from '$lib/api/client';
  import { previewChange, previewSentence } from '$lib/live/save.svelte';
  import { common, stations as copy } from '$lib/copy';
  import { shortAgo } from '$lib/format';
  import { config } from '$lib/live/config.svelte';
  import { saves } from '$lib/live/save.svelte';
  import { stream } from '$lib/sse/stream.svelte';
  import StatusBadge from '$lib/components/StatusBadge.svelte';

  $effect(() => {
    void config.load();
  });

  let busy = $state<string | null>(null);

  /**
   * Turning collection **off** is previewed and confirmed; turning it on is not.
   *
   * §2.4's sequence applies to this toggle exactly as it applies to a form, and leaving it
   * out was not a simplification -- it was the one unconfirmed, immediately-committed
   * configuration change in the whole application, sitting in a table row. On the preview
   * instance one stray click on it stopped three components, including the public website
   * and the frozen API the phone app uses, and **nothing on screen said so**: the operator
   * was left with a station that had simply stopped working.
   *
   * Asymmetric on purpose. Turning a station on is additive and reversible by the same
   * click; turning it off stops things, and §8.7's rule is that a change which breaks
   * something else says so first. The preview sentence comes from the same `plan()` dry
   * run every other save on this surface uses.
   */
  let confirmOpen = $state(false);
  let confirmOff = $state<{ key: string; name: string; sentence: string } | null>(null);
  let confirming = $state<string | null>(null);

  /**
   * Bumped whenever a change is declined or fails, to force the switch back.
   *
   * chonky's `Toggle` binds `checked` internally, so a click moves the switch whatever the
   * caller does next -- and declining the confirmation left it showing "off" while the
   * station was still on. The **next** click then read as *turn on*, which is how a
   * confirmation dialog becomes a way to silently do the opposite of what it asked about.
   * Keying the control on the configuration plus this counter recreates it with the right
   * state, which is the only handle a consumer has over a component that owns its own.
   * Reported upstream as gap G18.
   */
  let settled = $state(0);

  function decline() {
    confirmOpen = false;
    settled += 1;
  }

  async function ask(key: string, name: string) {
    confirming = key;
    try {
      const sentence = previewSentence(
        await previewChange({ change: 'put_station', key, station: toInput(key, false) })
      );
      confirmOff = { key, name, sentence };
      confirmOpen = true;
    } catch (e) {
      // A preview that cannot be taken is not a reason to refuse the change, but it is a
      // reason not to pretend we know what it does.
      confirmOff = { key, name, sentence: asFailure(e).summary };
      confirmOpen = true;
    } finally {
      confirming = null;
    }
  }

  /** The component that belongs to a station, for its badge. */
  function componentFor(key: string) {
    return stream.components.find(
      (c) => c.display.section === 'stations' && c.id.endsWith(`:${key}`)
    );
  }

  async function toggle(key: string, collecting: boolean) {
    confirmOpen = false;
    busy = key;
    try {
      const applied = unwrap(
        await api.PUT('/api/v1/stations/{key}', {
          params: { path: { key } },
          body: { ...toInput(key, collecting), collecting }
        })
      );
      saves.accept(applied);
      await config.refresh();
      // The banner in the shell reports the reconciliation; this says what was asked for,
      // because a toggle that flips back after a failed save is otherwise indistinguishable
      // from one that was never clicked.
      toast(
        collecting ? 'Collecting readings.' : 'Stopped collecting readings.',
        collecting ? 'success' : 'info'
      );
    } catch (e) {
      toast(asFailure(e).summary, 'danger');
      // The switch moved when it was clicked; a refused write must move it back, or the
      // screen says the opposite of what is stored.
      settled += 1;
    } finally {
      busy = null;
    }
  }

  /**
   * A `PUT` replaces the station, so the toggle has to send the rest of it back. Built
   * from the shared configuration read rather than from a form, which is what stops an
   * inline toggle quietly reverting a field edited in another tab.
   */
  function toInput(key: string, collecting: boolean) {
    const s = config.station(key);
    if (!s) throw new Error(`no station ${key}`);
    return {
      name: s.name,
      kind: s.kind,
      identity_kind: s.identity_kind,
      collecting,
      latitude: s.latitude,
      longitude: s.longitude,
      altitude_m: s.altitude_m,
      timezone: s.timezone,
      wind_dir_correction_degrees: s.wind_dir_correction_degrees,
      snow_base_distance_mm: s.snow_base_distance_mm,
      poll_interval_seconds: s.poll_interval_seconds,
      aprs_callsign: s.aprs_callsign,
      sensor_mapping: s.sensor_mapping,
      presented_key: s.presented_key,
      transport: {
        kind: s.transport.kind,
        host: s.transport.host,
        port: s.transport.port,
        device: s.transport.device,
        baud: s.transport.baud,
        method: s.transport.method,
        url: s.transport.url,
        topic: s.transport.topic,
        group: s.transport.group,
        mount: s.transport.mount,
        streaming: s.transport.streaming,
        tls_ca_path: s.transport.tls_ca_path,
        tls_cert_path: s.transport.tls_cert_path,
        tls_key_path: s.transport.tls_key_path
      }
    };
  }
</script>

<header class="pagehead">
  <div>
    <h1>{copy.list.title}</h1>
    <p class="blurb">{copy.list.blurb}</p>
  </div>
  <a class="btn btn-primary" href="/stations/new">{copy.list.addButton}</a>
</header>

<Box>
  {#if config.stations.length === 0}
    <EmptyState>
      <h3>{copy.list.empty.heading}</h3>
      <p>{copy.list.empty.body}</p>
      <a class="btn btn-primary" href="/stations/new">{copy.list.empty.action}</a>
    </EmptyState>
  {:else}
    <Table>
      <thead>
        <tr>
          <th>{copy.list.columns.name}</th>
          <th>{copy.list.columns.kind}</th>
          <th>{copy.list.columns.collecting}</th>
          <th>{copy.list.columns.status}</th>
          <th>{copy.list.columns.lastReading}</th>
        </tr>
      </thead>
      <tbody>
        {#each config.stations as station (station.key)}
          {@const component = componentFor(station.key)}
          {@const reading = stream.readings[station.key]}
          {@const received = station.kind === 'grpcreceiver'}
          <tr>
            <td>
              <a href={`/stations/${station.key}`}>{station.name}</a>
              {#if received}
                <Badge variant="info">{copy.list.receivedFrom(station.transport.host ?? 'a peer')}</Badge>
              {/if}
            </td>
            <td>{station.kind_display}</td>
            <td>
              <!--
                **No `label` on the switch.** chonky's `Toggle` wraps the control and its
                label in one `<label>`, so the words become part of the hit area -- and the
                column header already says what the switch is. A phrase in a table cell
                that silently stops a weather station when brushed is not a label.
              -->
              {#key `${station.key}:${station.collecting}:${settled}`}
                <Toggle
                  checked={station.collecting}
                  disabled={busy === station.key || confirming === station.key}
                  aria-label={`${copy.list.columns.collecting} for ${station.name}`}
                  onCheckedChange={(next) =>
                    next ? toggle(station.key, true) : ask(station.key, station.name)}
                />
              {/key}
            </td>
            <td>
              {#if component}
                <StatusBadge badge={component.badge} state={component.state} live={Boolean(reading)} />
              {:else if received}
                <span class="dim">{copy.list.receivedNote}</span>
              {:else}
                <span class="dim">{common.unknown}</span>
              {/if}
            </td>
            <td>{reading ? shortAgo(reading.ts, stream.now) : '—'}</td>
          </tr>
        {/each}
      </tbody>
    </Table>
  {/if}
</Box>

<!-- §8.7: a change that stops something else says so before it does it. -->
<!--
  `bind:open` and a separate target, not `open={target !== null}`. `AlertDialog.Root` takes
  a bindable `open` and offers no change callback (§6.3 gap G16), so a one-way prop and the
  component's own internal state drift apart the moment it is dismissed -- and the dialog
  then refuses to reopen for the rest of the page's life.
-->
<AlertDialog.Root bind:open={confirmOpen}>
  <AlertDialog.Content>
    <AlertDialog.Title>{copy.list.stopTitle}</AlertDialog.Title>
    <AlertDialog.Description>
      {copy.list.stopBody(confirmOff?.name ?? '')}
      {#if confirmOff}
        <span class="consequence">{confirmOff.sentence}</span>
      {/if}
    </AlertDialog.Description>
    <Button variant="default" onclick={decline}>{common.cancel}</Button>
    <Button variant="danger" onclick={() => confirmOff && toggle(confirmOff.key, false)}>
      {copy.list.stopConfirm}
    </Button>
  </AlertDialog.Content>
</AlertDialog.Root>

<style>
  .consequence {
    display: block;
    margin-block-start: 0.5rem;
    color: var(--color-text-muted);
  }

  .pagehead {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: 1rem;
    flex-wrap: wrap;
    margin-block-end: 1rem;
  }

  h1 {
    margin: 0 0 0.25rem;
    font-size: var(--text-xl);
  }

  .blurb {
    margin: 0;
    color: var(--color-text-muted);
    font-size: var(--text-sm);
  }

  .dim {
    color: var(--color-text-muted);
    font-size: var(--text-xs);
  }
</style>
