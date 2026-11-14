<!--
  §7.1's "Website — Extras": the radar map's registration, whose failure is a remote call's
  failure and is shown inline rather than as a save error.
-->
<script lang="ts">
  import { Badge, Box, Button, Input, toast } from '@chrissnell/chonky-ui';
  import { page } from '$app/state';
  import { api, asFailure, unwrap, type ApiFailure } from '$lib/api/client';
  import { common, website as copy } from '$lib/copy';
  import { stamp } from '$lib/format';
  import Field from '$lib/components/Field.svelte';
  import { config } from '$lib/live/config.svelte';
  import { saves } from '$lib/live/save.svelte';

  const id = $derived(Number(page.params.id));
  const site = $derived(config.website(id));

  let token = $state('');
  let busy = $state(false);
  let failure = $state<ApiFailure | null>(null);

  $effect(() => {
    if (!config.value) void config.load();
  });

  async function register() {
    busy = true;
    failure = null;
    try {
      const applied = unwrap(
        await api.POST('/api/v1/websites/{id}/radar', {
          params: { path: { id } },
          body: { token }
        })
      );
      saves.accept(applied);
      await config.refresh();
      token = '';
      toast('Registered.', 'success');
    } catch (e) {
      failure = asFailure(e);
    } finally {
      busy = false;
    }
  }

  async function turnOff() {
    busy = true;
    failure = null;
    try {
      const applied = unwrap(
        await api.DELETE('/api/v1/websites/{id}/radar', { params: { path: { id } } })
      );
      saves.accept(applied);
      await config.refresh();
    } catch (e) {
      failure = asFailure(e);
    } finally {
      busy = false;
    }
  }
</script>

<Box title={copy.extras.radarHeading}>
  <p class="blurb">{copy.extras.radarBlurb}</p>

  {#if site?.radar_enabled}
    <p class="row">
      <Badge variant="success">{common.on}</Badge>
      {#if site.radar_registered_at}
        <span class="dim">{copy.extras.registeredOn(stamp(site.radar_registered_at))}</span>
      {/if}
    </p>
  {:else}
    <p class="row">
      <Badge variant="default">{common.off}</Badge>
      <span class="dim">{copy.extras.notRegistered}</span>
    </p>
  {/if}

  <Field
    copy={{
      label: 'Registration token',
      hint: 'The token the radar provider issued for this website address.'
    }}
    error={failure?.fields.get('token')}
  >
    {#snippet children({ id: fid, describedBy, invalid })}
      <Input
        id={fid}
        value={token}
        oninput={(e) => (token = (e.currentTarget as HTMLInputElement).value)}
        aria-describedby={describedBy}
        aria-invalid={invalid}
        autocomplete="off"
      />
    {/snippet}
  </Field>

  {#if failure}
    <p class="error" role="alert">{copy.extras.registerFailed} {failure.summary}</p>
  {/if}

  <div class="actions">
    <Button variant="primary" onclick={register} disabled={busy || !token}>
      {site?.radar_enabled ? copy.extras.reregister : copy.extras.register}
    </Button>
    {#if site?.radar_enabled}
      <Button variant="default" onclick={turnOff} disabled={busy}>{copy.extras.turnOff}</Button>
    {/if}
  </div>
</Box>

<style>
  .blurb,
  .dim {
    margin: 0 0 0.75rem;
    color: var(--color-text-muted);
    font-size: var(--text-sm);
    max-width: 70ch;
  }

  .row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    margin: 0 0 0.75rem;
  }

  .row .dim {
    margin: 0;
  }

  .actions {
    display: flex;
    gap: 0.75rem;
  }

  .error {
    color: var(--color-danger);
    font-size: var(--text-sm);
  }
</style>
