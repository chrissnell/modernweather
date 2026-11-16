<!--
  §6.3 gap **G6**: chonky has a `copy` icon and no clipboard affordance.

  Three values in this application are shown once and must be copied exactly -- the
  one-time setup code, a peer certificate fingerprint, and the raw configuration download.
  Typing a 52-character base32 code by hand is how an operator ends up believing the code
  is wrong. Generic enough to propose upstream as-is.

  The fallback matters more than it looks: `navigator.clipboard` requires a secure context,
  and this admin screen is routinely served over plain http on a LAN -- the exact case
  §6.5's "no CDN" prohibition exists for. So the text is always selectable and the button
  says what happened rather than silently doing nothing.
-->
<script lang="ts">
  import { Button, Icon } from '@chrissnell/chonky-ui';
  import { common } from '$lib/copy';

  interface Props {
    value: string;
    /** An accessible name, since the value itself is usually opaque. */
    label: string;
    /** Break a long code into groups, which is how a person reads one back. */
    groupsOf?: number;
  }

  let { value, label, groupsOf }: Props = $props();

  let copied = $state(false);
  let failed = $state(false);

  const shown = $derived.by(() => {
    if (!groupsOf) return value;
    return (value.match(new RegExp(`.{1,${groupsOf}}`, 'g')) ?? [value]).join(' ');
  });

  async function copy() {
    failed = false;
    try {
      await navigator.clipboard.writeText(value);
      copied = true;
      setTimeout(() => (copied = false), 2_000);
    } catch {
      // No secure context, or no permission. Select it instead of pretending.
      failed = true;
    }
  }
</script>

<div class="copyfield">
  <code aria-label={label}>{shown}</code>
  <Button variant="ghost" size="sm" onclick={copy} aria-label={`${common.copy} ${label}`}>
    <Icon name="copy" size="sm" />
    {copied ? common.copied : common.copy}
  </Button>
</div>
{#if failed}
  <p class="fallback">
    This browser will not let a page use the clipboard over an unencrypted connection.
    Select the text above and copy it.
  </p>
{/if}

<style>
  .copyfield {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    flex-wrap: wrap;
  }

  code {
    user-select: all;
    padding: 0.35rem 0.5rem;
    border: 1px solid var(--color-border);
    border-radius: var(--radius);
    background: var(--color-surface-raised);
    font-size: var(--text-base);
    letter-spacing: 0.04em;
    word-break: break-all;
  }

  .fallback {
    margin: 0.25rem 0 0;
    font-size: var(--text-sm);
    color: var(--color-text-muted);
    max-width: 60ch;
  }
</style>
