<!--
  §0.3's deliberate exception: this page keeps `Callsign`, `Passcode`, `Symbol` and
  `Digipeater path` verbatim, because a person configuring it has a callsign and a
  passcode and those are the words on every other APRS tool and on the licence. "Share
  with amateur radio" is the *section* heading; "APRS / CWOP" is the service name.

  `plan()` owns the one constraint a form cannot check: a TNC serial port is exclusive, so
  pointing this at a port a Davis console already uses is a save-time error on the field.
-->
<script lang="ts">
  import { Box, Button, Input, Select, Table, Toggle, toast } from '@chrissnell/chonky-ui';
  import { api, asFailure, unwrap, type ApiFailure } from '$lib/api/client';
  import type { Schemas } from '$lib/api/client';
  import { common, share as copy } from '$lib/copy';
  import Field from '$lib/components/Field.svelte';
  import StatusBadge from '$lib/components/StatusBadge.svelte';
  import ScheduleLine from '$lib/components/ScheduleLine.svelte';
  import { config } from '$lib/live/config.svelte';
  import { previewChange, previewSentence, saves } from '$lib/live/save.svelte';
  import { stream } from '$lib/sse/stream.svelte';

  const SERVICE = 'aprs';
  const view = $derived(config.service(SERVICE));

  let enabled = $state(false);
  let passcode = $state('');
  let symbolTable = $state('/');
  let symbolCode = $state('_');
  let comment = $state('');
  let interval = $state('');
  let transport = $state('aprs-is');
  let server = $state('');
  let kissDevice = $state('');
  let kissBaud = $state('');
  let kissTcp = $state('');
  let kissPath = $state('');
  let kissDestination = $state('');
  let pairings = $state<Record<string, boolean>>({});
  let loadedFor = $state<string | null>(null);

  let busy = $state(false);
  let failure = $state<ApiFailure | null>(null);
  let preview = $state<string | null>(null);
  let ports = $state<Schemas['SerialPort'][]>([]);

  $effect(() => {
    if (!config.value) void config.load();
  });

  $effect(() => {
    if (!view || loadedFor === `${config.generation}`) return;
    enabled = view.enabled;
    symbolTable = view.aprs_symbol_table ?? '/';
    symbolCode = view.aprs_symbol_code ?? '_';
    comment = view.aprs_comment ?? '';
    interval = view.interval_seconds?.toString() ?? '';
    transport = view.aprs_transport ?? 'aprs-is';
    server = view.aprs_server ?? '';
    kissDevice = view.aprs_kiss_serial_device ?? '';
    kissBaud = view.aprs_kiss_serial_baud?.toString() ?? '';
    kissTcp = view.aprs_kiss_tcp_address ?? '';
    kissPath = view.aprs_kiss_path ?? '';
    kissDestination = view.aprs_kiss_destination ?? '';
    passcode = '';
    pairings = Object.fromEntries(view.stations.map((p) => [p.station_key, true]));
    loadedFor = `${config.generation}`;
  });

  $effect(() => {
    if (transport !== 'kiss-serial') return;
    void (async () => {
      try {
        ports = unwrap(await api.GET('/api/v1/system/serial-ports')).ports;
      } catch {
        ports = [];
      }
    })();
  });

  const component = $derived(stream.components.find((c) => c.id.startsWith('aprs:')));

  const callsigns = $derived(
    config.stations.filter((s) => s.aprs_callsign).map((s) => ({ key: s.key, name: s.name, callsign: s.aprs_callsign! }))
  );

  const intervalWarning = $derived.by(() => {
    const n = Number(interval);
    if (!interval || !Number.isFinite(n)) return undefined;
    return n < 300 ? 'APRS asks for no more than one beacon every 300 seconds.' : undefined;
  });

  function build(): Schemas['ServiceInput'] {
    return {
      name: view?.name ?? SERVICE,
      enabled,
      passcode: passcode === '' ? null : passcode,
      interval_seconds: interval === '' ? null : Number(interval),
      aprs_symbol_table: symbolTable || null,
      aprs_symbol_code: symbolCode || null,
      aprs_comment: comment || null,
      aprs_transport: transport,
      aprs_server: transport === 'aprs-is' ? server || null : null,
      aprs_kiss_serial_device: transport === 'kiss-serial' ? kissDevice || null : null,
      aprs_kiss_serial_baud:
        transport === 'kiss-serial' && kissBaud ? Number(kissBaud) : null,
      aprs_kiss_tcp_address: transport === 'kiss-tcp' ? kissTcp || null : null,
      aprs_kiss_path: kissPath || null,
      aprs_kiss_destination: kissDestination || null,
      stations: Object.entries(pairings)
        .filter(([, on]) => on)
        .map(([station_key]) => ({ station_key }))
    };
  }

  async function propose() {
    failure = null;
    preview = null;
    try {
      preview = previewSentence(
        await previewChange({ change: 'put_service', service: SERVICE, settings: build() })
      );
    } catch (e) {
      failure = asFailure(e);
    }
  }

  async function save() {
    busy = true;
    failure = null;
    try {
      const applied = unwrap(
        await api.PUT('/api/v1/services/{service}', {
          params: { path: { service: SERVICE } },
          body: build()
        })
      );
      saves.accept(applied);
      await config.refresh();
      preview = null;
      toast('Saved.', 'success');
    } catch (e) {
      failure = asFailure(e);
    } finally {
      busy = false;
    }
  }

  const err = (p: string) => failure?.fields.get(p);
</script>

<h1>APRS / CWOP</h1>
<p class="blurb">Beacon your readings over APRS-IS or a radio.</p>

<Box title={copy.upload.statusSection}>
  {#if component}
    <StatusBadge badge={component.badge} state={component.state} sentence={component.sentence} />
    <ScheduleLine schedule={component.schedule} now={stream.now} />
    {#if component.last_error}
      <pre>{component.last_error}</pre>
    {/if}
  {:else}
    <p class="dim">Nothing is beaconing yet.</p>
  {/if}
</Box>

<Box title="Account">
  <Field copy={copy.fields.enabled}>
    {#snippet children({ id, describedBy })}
      <Toggle
        id={id}
        checked={enabled}
        label={copy.fields.enabled.label}
        aria-describedby={describedBy}
        onCheckedChange={(n) => (enabled = n)}
      />
    {/snippet}
  </Field>

  <Field
    copy={copy.fields.passcode}
    note={view?.passcode.is_set ? 'A passcode is stored. Leave empty to keep it.' : undefined}
    error={err('passcode')}
  >
    {#snippet children({ id, describedBy, invalid })}
      <Input
        {id}
        type="password"
        value={passcode}
        oninput={(e) => (passcode = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
        aria-invalid={invalid}
        inputmode="numeric"
        autocomplete="off"
      />
    {/snippet}
  </Field>

  <Field copy={copy.fields.interval} note="seconds" error={err('interval_seconds') ?? intervalWarning}>
    {#snippet children({ id, describedBy })}
      <Input
        {id}
        type="number"
        value={interval}
        oninput={(e) => (interval = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
      />
    {/snippet}
  </Field>

  <Field copy={copy.fields.symbolTable} error={err('aprs_symbol_table')}>
    {#snippet children({ id, describedBy })}
      <Input
        {id}
        value={symbolTable}
        oninput={(e) => (symbolTable = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
        maxlength={1}
      />
    {/snippet}
  </Field>

  <Field copy={copy.fields.symbolCode} error={err('aprs_symbol_code')}>
    {#snippet children({ id, describedBy })}
      <Input
        {id}
        value={symbolCode}
        oninput={(e) => (symbolCode = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
        maxlength={1}
      />
    {/snippet}
  </Field>

  <Field copy={copy.fields.comment} error={err('aprs_comment')}>
    {#snippet children({ id, describedBy })}
      <Input
        {id}
        value={comment}
        oninput={(e) => (comment = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
      />
    {/snippet}
  </Field>
</Box>

<Box title="How to send it">
  <Field copy={copy.fields.aprsTransport}>
    {#snippet children({ id, describedBy })}
      <Select
        id={id}
        aria-describedby={describedBy}
        value={transport}
        onValueChange={(v) => (transport = v)}
        options={[
          { value: 'aprs-is', label: 'APRS-IS (over the internet)' },
          { value: 'kiss-serial', label: 'A TNC radio, by cable' },
          { value: 'kiss-tcp', label: 'A TNC radio, over the network' }
        ]}
      />
    {/snippet}
  </Field>

  {#if transport === 'aprs-is'}
    <Field copy={copy.fields.aprsServer} error={err('aprs_server')}>
      {#snippet children({ id, describedBy })}
        <Input
          {id}
          value={server}
          oninput={(e) => (server = (e.currentTarget as HTMLInputElement).value)}
          aria-describedby={describedBy}
        />
      {/snippet}
    </Field>
  {:else if transport === 'kiss-serial'}
    <Field copy={copy.fields.kissSerialDevice} error={err('aprs_kiss_serial_device')}>
      {#snippet children({ id, describedBy, invalid })}
        {#if ports.length > 0}
          <Select
            id={id}
            aria-describedby={describedBy}
            value={kissDevice}
            onValueChange={(v) => (kissDevice = v)}
            options={ports.map((p) => ({
              value: p.stable_device ?? p.device,
              label: p.description
                ? `${p.stable_device ?? p.device} — ${p.description}`
                : (p.stable_device ?? p.device)
            }))}
          />
        {:else}
          <Input
            {id}
            value={kissDevice}
            oninput={(e) => (kissDevice = (e.currentTarget as HTMLInputElement).value)}
            aria-describedby={describedBy}
            aria-invalid={invalid}
          />
        {/if}
      {/snippet}
    </Field>
    <Field copy={copy.fields.kissSerialBaud} error={err('aprs_kiss_serial_baud')}>
      {#snippet children({ id, describedBy })}
        <Input
          {id}
          type="number"
          value={kissBaud}
          oninput={(e) => (kissBaud = (e.currentTarget as HTMLInputElement).value)}
          aria-describedby={describedBy}
        />
      {/snippet}
    </Field>
    <Field copy={copy.fields.kissPath} error={err('aprs_kiss_path')}>
      {#snippet children({ id, describedBy })}
        <Input
          {id}
          value={kissPath}
          oninput={(e) => (kissPath = (e.currentTarget as HTMLInputElement).value)}
          aria-describedby={describedBy}
        />
      {/snippet}
    </Field>
    <Field copy={copy.fields.kissDestination} error={err('aprs_kiss_destination')}>
      {#snippet children({ id, describedBy })}
        <Input
          {id}
          value={kissDestination}
          oninput={(e) => (kissDestination = (e.currentTarget as HTMLInputElement).value)}
          aria-describedby={describedBy}
        />
      {/snippet}
    </Field>
  {:else}
    <Field copy={copy.fields.kissTcpAddress} error={err('aprs_kiss_tcp_address')}>
      {#snippet children({ id, describedBy, invalid })}
        <Input
          {id}
          value={kissTcp}
          oninput={(e) => (kissTcp = (e.currentTarget as HTMLInputElement).value)}
          aria-describedby={describedBy}
          aria-invalid={invalid}
        />
      {/snippet}
    </Field>
    <Field copy={copy.fields.kissPath} error={err('aprs_kiss_path')}>
      {#snippet children({ id, describedBy })}
        <Input
          {id}
          value={kissPath}
          oninput={(e) => (kissPath = (e.currentTarget as HTMLInputElement).value)}
          aria-describedby={describedBy}
        />
      {/snippet}
    </Field>
    <Field copy={copy.fields.kissDestination} error={err('aprs_kiss_destination')}>
      {#snippet children({ id, describedBy })}
        <Input
          {id}
          value={kissDestination}
          oninput={(e) => (kissDestination = (e.currentTarget as HTMLInputElement).value)}
          aria-describedby={describedBy}
        />
      {/snippet}
    </Field>
  {/if}
</Box>

<Box title={copy.upload.perStationSection}>
  {#if callsigns.length === 0}
    <p class="dim">
      No station has a callsign yet. A callsign is set on the station's own settings page,
      because it is the station's public identity rather than this service's.
    </p>
  {:else}
    <Table>
      <thead>
        <tr><th>Station</th><th>{copy.fields.callsign.label}</th><th>Beacon it</th></tr>
      </thead>
      <tbody>
        {#each callsigns as s (s.key)}
          <tr>
            <td>{s.name}</td>
            <td>{s.callsign}</td>
            <td>
              <Toggle
                checked={pairings[s.key] ?? false}
                label={`Beacon ${s.name}`}
                onCheckedChange={(n) => (pairings = { ...pairings, [s.key]: n })}
              />
            </td>
          </tr>
        {/each}
      </tbody>
    </Table>
  {/if}
</Box>

{#if failure}
  <!-- Always, not only when no field error came with it: a `plan()` error about an
       exclusive serial port names a path this form has no control for, and suppressing the
       summary would make Save look like it did nothing. -->
  <p class="error" role="alert">{failure.summary}</p>
{/if}

{#if preview}
  <Box>
    <p class="preview">{preview}</p>
    <div class="actions">
      <Button variant="default" onclick={() => (preview = null)}>{common.cancel}</Button>
      <Button variant="primary" onclick={save} disabled={busy}>
        {busy ? common.saving : common.save}
      </Button>
    </div>
  </Box>
{:else}
  <div class="actions">
    <Button variant="primary" onclick={propose} disabled={busy}>{common.save}</Button>
  </div>
{/if}

<style>
  h1 {
    margin: 0 0 0.25rem;
    font-size: var(--text-xl);
  }

  .blurb,
  .dim {
    margin: 0 0 0.75rem;
    color: var(--color-text-muted);
    font-size: var(--text-sm);
    max-width: 75ch;
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

  .preview {
    margin: 0 0 0.75rem;
    font-size: var(--text-sm);
  }

  .actions {
    display: flex;
    gap: 0.75rem;
    margin-block-start: 1rem;
  }

  .error {
    color: var(--color-danger);
    font-size: var(--text-sm);
  }
</style>
