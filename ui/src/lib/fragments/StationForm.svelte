<!--
  §0.7 step 2 and §7.1's "Station — Settings", which are the same form.

  Three things it does that the current UI does not, each a §7.3 entry:

  * **type first, then only that type's fields** (`$lib/live/stationform.ts` owns the
    table). Today's form shows all 50-plus `DeviceData` columns for every kind, so an
    AirGradient sensor is asked for a radio path.
  * **serial ports are enumerated, never typed**, with an empty list that names its cause
    rather than reading as "you have no serial ports".
  * **Test connection is the primary action and Save is enabled regardless** (§0.7). A
    failed test does not block a save -- configuring a station that is not plugged in yet
    is legitimate -- but the operator sees the failure before walking away believing it
    worked.

  The four fields M5-2 made non-negotiable are here and are not optional extras:
  `wind_dir_correction_degrees`, `latitude`, `longitude`, `altitude_m`. M5-2 measured the
  Go configuration's -94 degrees against this side's 0 and it made `winddir` differ on
  100% of readings with nothing downstream able to tell.
-->
<script lang="ts">
  import { Box, Button, Input, RadioGroup, Radio, Select, Toggle } from '@chrissnell/chonky-ui';
  import { untrack } from 'svelte';
  import { api, asFailure, unwrap, type ApiFailure } from '$lib/api/client';
  import type { Schemas } from '$lib/api/client';
  import { common, receivedFromPeer, stationKinds, stations as copy } from '$lib/copy';
  import Field from '$lib/components/Field.svelte';
  import { shapeFor } from '$lib/live/stationform';

  interface Props {
    /** Absent on a create. */
    station?: Schemas['StationView'] | null;
    busy?: boolean;
    failure?: ApiFailure | null;
    onsubmit: (input: Schemas['StationInput']) => void;
  }

  let { station = null, busy = false, failure = null, onsubmit }: Props = $props();

  const creating = $derived(station === null);

  /**
   * The form is seeded **once**, on purpose, and `untrack` says so rather than leaving
   * `svelte-check` to warn about it: a form that re-reads its own fields from the server
   * while the operator is typing discards what they typed. The route remounts when the
   * station changes, which is what makes one seeding correct.
   */
  const seed = untrack(() => station);

  let kind = $state(seed?.kind ?? '');
  let name = $state(seed?.name ?? '');
  let key = $state(seed?.key ?? '');
  let collecting = $state(seed?.collecting ?? true);
  let transportKind = $state(seed?.transport.kind ?? '');
  let host = $state(seed?.transport.host ?? '');
  let port = $state(seed?.transport.port?.toString() ?? '');
  let device = $state(seed?.transport.device ?? '');
  let baud = $state(seed?.transport.baud?.toString() ?? '');
  let url = $state(seed?.transport.url ?? '');
  let mount = $state(seed?.transport.mount ?? '');
  // The three `TransportInput` fields a form has no control for, carried through a save
  // so that an edit does not null them. A `PUT` replaces the station, so a field this
  // form does not send is a field the store loses -- which for `topic` on an MQTT
  // station is the whole connection.
  let topic = $state(seed?.transport.topic ?? '');
  let group = $state(seed?.transport.group ?? '');
  let tlsCa = $state(seed?.transport.tls_ca_path ?? '');
  let tlsCert = $state(seed?.transport.tls_cert_path ?? '');
  let tlsKey = $state(seed?.transport.tls_key_path ?? '');
  let method = $state(seed?.transport.method ?? '');
  let latitude = $state(seed?.latitude?.toString() ?? '');
  let longitude = $state(seed?.longitude?.toString() ?? '');
  let altitude = $state(seed?.altitude_m?.toString() ?? '');
  let timezone = $state(seed?.timezone ?? '');
  let windCorrection = $state((seed?.wind_dir_correction_degrees ?? 0).toString());
  let snowBase = $state(seed?.snow_base_distance_mm?.toString() ?? '');
  let pollInterval = $state(seed?.poll_interval_seconds?.toString() ?? '');
  let callsign = $state(seed?.aprs_callsign ?? '');
  let sensorMapping = $state(seed?.sensor_mapping ?? '');
  let presentedKey = $state(seed?.presented_key ?? '');

  const shape = $derived(shapeFor(kind));

  // Picking a kind on a create sets the connection that kind actually uses, so the
  // operator is not asked to choose between transports they have never heard of.
  $effect(() => {
    if (creating && kind && !transportKind) {
      transportKind = shape.defaultTransport;
      if (shape.defaultBaud && !baud) baud = String(shape.defaultBaud);
    }
  });

  let ports = $state<Schemas['SerialPort'][]>([]);
  let portsError = $state<string | null>(null);

  $effect(() => {
    if (shape.connection !== 'serial-or-network') return;
    void (async () => {
      try {
        const view = unwrap(await api.GET('/api/v1/system/serial-ports'));
        ports = view.ports;
        portsError = null;
      } catch (e) {
        ports = [];
        portsError = asFailure(e).summary;
      }
    })();
  });

  const timezones = $derived.by(() => {
    // `Intl.supportedValuesOf` is the browser's own IANA list -- no vendored table and no
    // bytes in the bundle (§6.5). A browser without it gets a plain text field.
    try {
      return Intl.supportedValuesOf('timeZone');
    } catch {
      return [] as string[];
    }
  });

  function num(v: string): number | null {
    if (v.trim() === '') return null;
    const n = Number(v);
    return Number.isFinite(n) ? n : null;
  }

  function build(): Schemas['StationInput'] {
    return {
      name,
      kind,
      identity_kind: station?.identity_kind ?? (shape.presentsIdentity ? 'from_query' : 'fixed'),
      key: creating ? key || undefined : undefined,
      collecting,
      latitude: shape.located ? num(latitude) : null,
      longitude: shape.located ? num(longitude) : null,
      altitude_m: shape.located ? num(altitude) : null,
      timezone: timezone || null,
      wind_dir_correction_degrees: shape.wind ? (num(windCorrection) ?? 0) : 0,
      snow_base_distance_mm: shape.snow ? num(snowBase) : null,
      poll_interval_seconds: shape.polled ? num(pollInterval) : null,
      aprs_callsign: callsign || null,
      sensor_mapping: shape.sensorMapping ? sensorMapping || null : null,
      presented_key: shape.presentsIdentity ? presentedKey || null : null,
      transport: {
        kind: transportKind || null,
        host: host || null,
        port: num(port),
        device: device || null,
        baud: num(baud),
        url: url || null,
        mount: mount || null,
        // `streaming` belongs to **`grpc_out` alone**. W2a §4a.3 is `HttpOut { auth }` and
        // `GrpcOut { streaming: bool }`, and the configuration database enforces exactly
        // that -- so sending it with an `http_out` station is a refusal, not a default.
        // The value is a fact about the hardware rather than a choice, which is why it
        // comes from the kind table and not from a control.
        streaming: transportKind === 'grpc_out' ? shape.streams : null,
        // The rest are carried rather than offered: this form has no control for them, and
        // a `PUT` that omitted them would null whatever is stored.
        topic: topic || null,
        group: group || null,
        method: method || null,
        tls_ca_path: tlsCa || null,
        tls_cert_path: tlsCert || null,
        tls_key_path: tlsKey || null
      }
    };
  }

  function submit(event: SubmitEvent) {
    event.preventDefault();
    onsubmit(build());
  }

  const err = (path: string) => failure?.fields.get(path);
</script>

<form onsubmit={submit}>
  {#if creating}
    <Box title={copy.add.chooserTitle}>
      <RadioGroup value={kind} onValueChange={(v) => (kind = v)}>
        {#each stationKinds as option (option.kind)}
          <div class="kind">
            <Radio value={option.kind} label={option.label} />
            <span class="how">{option.how}</span>
          </div>
        {/each}
      </RadioGroup>
      <!--
        §0.7's seventh row, which is a link rather than a type: a received station is a
        pairing plus whatever a peer registers, and the schema's `stations.type` constraint
        excludes it by name. Offered here because it is what the operator is looking for,
        and it goes to the page §0.6 puts the pairing on.
      -->
      <div class="kind">
        <a href={receivedFromPeer.href}>{receivedFromPeer.label}</a>
        <span class="how">{receivedFromPeer.how}</span>
      </div>
      <p class="note">{receivedFromPeer.note}</p>
      <p class="note">{copy.add.discoveryUnavailable}</p>
    </Box>
  {/if}

  {#if kind}
    <Box title={copy.settings.sections.identity}>
      <Field copy={copy.fields.name} required error={err('name')}>
        {#snippet children({ id, describedBy, invalid })}
          <Input
            {id}
            value={name}
            oninput={(e) => (name = (e.currentTarget as HTMLInputElement).value)}
            aria-describedby={describedBy}
            aria-invalid={invalid}
          />
        {/snippet}
      </Field>

      {#if creating}
        <Field
          copy={{
            label: 'Short name',
            hint: 'A short, lower-case name used in addresses and in the log. Letters, digits and dashes.'
          }}
          required
          error={err('key')}
        >
          {#snippet children({ id, describedBy, invalid })}
            <Input
              {id}
              value={key}
              oninput={(e) =>
                (key = (e.currentTarget as HTMLInputElement).value.toLowerCase().replace(/[^a-z0-9-]/g, ''))}
              aria-describedby={describedBy}
              aria-invalid={invalid}
            />
          {/snippet}
        </Field>
      {/if}

      <Field copy={copy.fields.collecting}>
        {#snippet children({ id, describedBy })}
          <Toggle
            id={id}
            checked={collecting}
            label={copy.fields.collecting.label}
            aria-describedby={describedBy}
            onCheckedChange={(next) => (collecting = next)}
          />
        {/snippet}
      </Field>
    </Box>

    <Box title={copy.settings.sections.connection}>
      {#if shape.connection === 'serial-or-network'}
        <Field
          copy={{ label: 'How it connects', hint: 'By cable, or over the network.' }}
          error={err('transport.kind')}
        >
          {#snippet children({ id, describedBy })}
            <Select
              id={id}
              aria-describedby={describedBy}
              value={transportKind}
              onValueChange={(v) => (transportKind = v)}
              options={[
                { value: 'serial', label: 'By cable' },
                { value: 'tcp_out', label: 'Over the network' }
              ]}
            />
          {/snippet}
        </Field>
      {/if}

      {#if transportKind === 'serial'}
        <Field copy={copy.fields.device} error={err('transport.device')}>
          {#snippet children({ id, describedBy })}
            {#if ports.length > 0}
              <Select
                id={id}
                aria-describedby={describedBy}
                value={device}
                onValueChange={(v) => (device = v)}
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
                value={device}
                oninput={(e) => (device = (e.currentTarget as HTMLInputElement).value)}
                aria-describedby={describedBy}
              />
            {/if}
          {/snippet}
        </Field>
        {#if ports.length === 0}
          <p class="note">
            {portsError ?? copy.add.noSerialPorts}
            <button class="linkish" type="button" onclick={() => (transportKind = 'tcp_out')}>
              {copy.add.chooseNetworkInstead}
            </button>
          </p>
        {/if}

        <Field copy={copy.fields.baud} error={err('transport.baud')}>
          {#snippet children({ id, describedBy })}
            <Input
              {id}
              type="number"
              value={baud}
              oninput={(e) => (baud = (e.currentTarget as HTMLInputElement).value)}
              aria-describedby={describedBy}
            />
          {/snippet}
        </Field>
      {/if}

      {#if transportKind === 'tcp_out'}
        <Field copy={copy.fields.host} error={err('transport.host')}>
          {#snippet children({ id, describedBy, invalid })}
            <Input
              {id}
              value={host}
              oninput={(e) => (host = (e.currentTarget as HTMLInputElement).value)}
              aria-describedby={describedBy}
              aria-invalid={invalid}
            />
          {/snippet}
        </Field>
        <Field copy={copy.fields.port} error={err('transport.port')}>
          {#snippet children({ id, describedBy, invalid })}
            <Input
              {id}
              type="number"
              value={port}
              oninput={(e) => (port = (e.currentTarget as HTMLInputElement).value)}
              aria-describedby={describedBy}
              aria-invalid={invalid}
            />
          {/snippet}
        </Field>
      {/if}

      {#if shape.connection === 'inbound'}
        <Field
          copy={{
            label: 'Where it sends to',
            hint: 'The path on this machine the station posts its readings to.'
          }}
          error={err('transport.mount')}
        >
          {#snippet children({ id, describedBy })}
            <Input
              {id}
              value={mount}
              oninput={(e) => (mount = (e.currentTarget as HTMLInputElement).value)}
              aria-describedby={describedBy}
            />
          {/snippet}
        </Field>
      {/if}

      {#if transportKind === 'http_out' || transportKind === 'grpc_out' || transportKind === 'mqtt_sub'}
        <Field
          copy={{ label: 'Address to read from', hint: 'The full address this sensor answers on.' }}
          error={err('transport.url')}
        >
          {#snippet children({ id, describedBy })}
            <Input
              {id}
              value={url}
              oninput={(e) => (url = (e.currentTarget as HTMLInputElement).value)}
              aria-describedby={describedBy}
            />
          {/snippet}
        </Field>
      {/if}

      {#if shape.polled}
        <Field copy={copy.fields.pollInterval} error={err('poll_interval_seconds')}>
          {#snippet children({ id, describedBy })}
            <Input
              {id}
              type="number"
              value={pollInterval}
              oninput={(e) => (pollInterval = (e.currentTarget as HTMLInputElement).value)}
              aria-describedby={describedBy}
            />
          {/snippet}
        </Field>
      {/if}

      {#if shape.presentsIdentity}
        <Field copy={copy.fields.presentedKey} error={err('presented_key')}>
          {#snippet children({ id, describedBy })}
            <Input
              {id}
              value={presentedKey}
              oninput={(e) => (presentedKey = (e.currentTarget as HTMLInputElement).value)}
              aria-describedby={describedBy}
            />
          {/snippet}
        </Field>
      {/if}

      {#if shape.sensorMapping}
        <Field copy={copy.fields.sensorMapping} error={err('sensor_mapping')}>
          {#snippet children({ id, describedBy })}
            <Input
              {id}
              value={sensorMapping}
              oninput={(e) => (sensorMapping = (e.currentTarget as HTMLInputElement).value)}
              aria-describedby={describedBy}
            />
          {/snippet}
        </Field>
      {/if}
    </Box>

    {#if shape.located}
      <Box title={copy.settings.sections.location}>
        <Field copy={copy.fields.latitude} error={err('latitude')}>
          {#snippet children({ id, describedBy, invalid })}
            <Input
              {id}
              value={latitude}
              oninput={(e) => (latitude = (e.currentTarget as HTMLInputElement).value)}
              aria-describedby={describedBy}
              aria-invalid={invalid}
              inputmode="decimal"
            />
          {/snippet}
        </Field>
        <Field copy={copy.fields.longitude} error={err('longitude')}>
          {#snippet children({ id, describedBy, invalid })}
            <Input
              {id}
              value={longitude}
              oninput={(e) => (longitude = (e.currentTarget as HTMLInputElement).value)}
              aria-describedby={describedBy}
              aria-invalid={invalid}
              inputmode="decimal"
            />
          {/snippet}
        </Field>
        <Field copy={copy.fields.altitude} note="metres" error={err('altitude_m')}>
          {#snippet children({ id, describedBy, invalid })}
            <Input
              {id}
              value={altitude}
              oninput={(e) => (altitude = (e.currentTarget as HTMLInputElement).value)}
              aria-describedby={describedBy}
              aria-invalid={invalid}
              inputmode="decimal"
            />
          {/snippet}
        </Field>
        <Field copy={copy.fields.timezone} error={err('timezone')}>
          {#snippet children({ id, describedBy, invalid })}
            <!--
              A native `input` + `datalist` rather than chonky's `Combobox`, and this is a
              **new finding** rather than a preference: `Combobox.Root` takes only `open`,
              `value` and `children`, and `Combobox.Input` takes only `class` and
              `placeholder` -- so it forwards neither an `id`, an `aria-describedby`, nor
              an input handler, and a field that cannot be labelled or described cannot
              satisfy §0.5 or §7.4. Reported as gap **G13**.

              The substitute is not a compromise here: a `datalist` is searchable,
              keyboard-reachable, screen-reader-announced, and costs zero bytes against
              §6.5's budget, and the list itself is the browser's own IANA database rather
              than a vendored table.
            -->
            <Input
              {id}
              value={timezone}
              oninput={(e) => (timezone = (e.currentTarget as HTMLInputElement).value)}
              aria-describedby={describedBy}
              aria-invalid={invalid}
              list="mw-timezones"
              autocomplete="off"
            />
            {#if timezones.length > 0}
              <datalist id="mw-timezones">
                {#each timezones as zone (zone)}
                  <option value={zone}></option>
                {/each}
              </datalist>
            {/if}
          {/snippet}
        </Field>
      </Box>
    {/if}

    {#if shape.wind || shape.snow}
      <Box title={copy.settings.sections.calibration}>
        {#if shape.wind}
          <Field
            copy={copy.fields.windDirCorrection}
            note="degrees"
            error={err('wind_dir_correction_degrees')}
          >
            {#snippet children({ id, describedBy, invalid })}
              <Input
                {id}
                type="number"
                value={windCorrection}
                oninput={(e) => (windCorrection = (e.currentTarget as HTMLInputElement).value)}
                aria-describedby={describedBy}
                aria-invalid={invalid}
              />
            {/snippet}
          </Field>
        {/if}
        {#if shape.snow}
          <Field
            copy={copy.fields.snowBaseDistance}
            note="millimetres"
            error={err('snow_base_distance_mm')}
          >
            {#snippet children({ id, describedBy })}
              <Input
                {id}
                type="number"
                value={snowBase}
                oninput={(e) => (snowBase = (e.currentTarget as HTMLInputElement).value)}
                aria-describedby={describedBy}
              />
            {/snippet}
          </Field>
        {/if}
        <Field copy={copy.fields.aprsCallsign} error={err('aprs_callsign')}>
          {#snippet children({ id, describedBy })}
            <Input
              {id}
              value={callsign}
              oninput={(e) => (callsign = (e.currentTarget as HTMLInputElement).value.toUpperCase())}
              aria-describedby={describedBy}
            />
          {/snippet}
        </Field>
      </Box>
    {/if}

    <!--
      **Shown whenever there is a failure, not only when no field error came with it.**
      Suppressing it when `fields` is non-empty assumes every field path has a control on
      this form to land on, and §7.2's paths include ones that do not -- `transport.*` on a
      kind whose form omits the field, or a `plan()` error about a different station. The
      operator then sees a Save that does nothing at all, which is the exact defect §2
      exists against.
    -->
    {#if failure}
      <p class="error" role="alert">{failure.summary}</p>
      {#if failure.fields.size > 0}
        <ul class="error">
          {#each [...failure.fields] as [field, message] (field)}
            <li>{message}</li>
          {/each}
        </ul>
      {/if}
    {/if}

    <div class="actions">
      <!-- §0.7: Test is the primary action and Save is enabled regardless. The check
           itself needs `POST /api/v1/checks/*`, which no work item creates yet, so the
           affordance says that rather than being a button that does nothing. -->
      <Button variant="default" disabled title={copy.add.testUnavailable}>
        {copy.add.testFirst}
      </Button>
      <Button type="submit" variant="primary" disabled={busy || !name || !kind}>
        {busy ? common.saving : common.save}
      </Button>
    </div>
    <p class="note">{copy.add.saveAnywayHint}</p>
    <p class="note">{copy.add.testUnavailable}</p>
  {/if}
</form>

<style>
  .kind {
    display: flex;
    align-items: baseline;
    gap: 0.75rem;
    padding-block: 0.2rem;
  }

  .how {
    font-size: var(--text-xs);
    color: var(--color-text-muted);
  }

  .note {
    margin: 0.25rem 0 0;
    font-size: var(--text-xs);
    color: var(--color-text-muted);
    max-width: 70ch;
  }

  .error {
    color: var(--color-danger);
    font-size: var(--text-sm);
  }

  .actions {
    display: flex;
    gap: 0.75rem;
    margin-block-start: 1rem;
    flex-wrap: wrap;
  }

  .linkish {
    background: none;
    border: none;
    padding: 0;
    color: var(--color-primary);
    cursor: pointer;
    font: inherit;
    text-decoration: underline;
  }
</style>
