<!--
  §6.3 gap **G4**, which the design calls the most valuable of the twelve, plus **G5**.

  chonky puts `error?: string` on `Input` and on nothing else -- not `Select`, `Toggle`,
  `Checkbox`, `Combobox`, `Slider` or `PinInput` -- and `Label` has no hint slot. Without
  one wrapper that owns label + hint + error + `aria-describedby`/`aria-invalid`, §8.5's
  field-level validation ends up expressed two ways: a prop on text inputs and something
  improvised everywhere else. That is the "two validation vocabularies" the gap note
  warns about, so the wrapper exists and **every** form control in this application goes
  through it.

  §0.5's standard is structural here rather than remembered: `hint` is a required prop, so
  a field without one does not compile.
-->
<script lang="ts">
  import { Collapsible, Label } from '@chrissnell/chonky-ui';
  import type { Snippet } from 'svelte';
  import { common } from '$lib/copy';
  import type { FieldCopy } from '$lib/copy';

  interface Props {
    copy: FieldCopy;
    /** The server's message for this field, by path -- §7.2's `stations[0].hostname`. */
    error?: string;
    /** Rendered after the hint: a unit, a current value, a consequence. */
    note?: string;
    /** Marked required only where the field is structurally unusable when empty. */
    required?: boolean;
    children: Snippet<[{ id: string; describedBy: string; invalid: boolean }]>;
  }

  let { copy, error, note, required = false, children }: Props = $props();

  // One id per instance, which is what lets the hint and the error be *announced* rather
  // than merely positioned near the control.
  const id = $props.id();
  const hintId = `${id}-hint`;
  const errorId = `${id}-error`;
  let open = $state(false);

  const invalid = $derived(Boolean(error));
  const describedBy = $derived(invalid ? `${hintId} ${errorId}` : hintId);
</script>

<div class="field">
  <!--
    The required marker is a **sibling** of the label rather than inside it, which is a
    testability fact as much as a semantic one: a label's text content is what both a
    screen reader and a test locator read, so "Password*" inside the element makes the
    field's name "Password*" and `getByLabel('Password', { exact: true })` finds nothing.
    `aria-hidden` keeps the glyph out of the accessible name either way.
  -->
  <span class="labelrow">
    <Label for={id}>{copy.label}</Label>
    {#if required}<span class="req" aria-hidden="true">*</span>{/if}
  </span>

  {@render children({ id, describedBy, invalid })}

  <p class="hint" id={hintId}>
    {copy.hint}{#if note}
      <span class="note">{note}</span>
    {/if}
  </p>

  {#if copy.more}
    <Collapsible.Root bind:open>
      <Collapsible.Trigger>
        {open ? common.less : common.more}
      </Collapsible.Trigger>
      <Collapsible.Content>
        <p class="more">{copy.more}</p>
      </Collapsible.Content>
    </Collapsible.Root>
  {/if}

  {#if error}
    <!--
      `alert` rather than `status`: a validation message that arrives after a save is the
      one thing on the page the operator has to read before doing anything else.
    -->
    <p class="error" id={errorId} role="alert">{error}</p>
  {/if}
</div>

<style>
  .field {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
    margin-block-end: 1rem;
  }

  /* §0.5: small, dim, and ALWAYS visible -- not revealed on hover, which does not exist
     on a phone, and an operator checking a station from the garden is a real case. */
  .hint {
    margin: 0;
    font-size: var(--text-xs);
    color: var(--color-text-muted);
  }

  .note {
    margin-inline-start: 0.5rem;
    color: var(--color-text-dim);
  }

  .more {
    margin: 0.25rem 0 0;
    font-size: var(--text-sm);
    color: var(--color-text-muted);
    max-width: 60ch;
  }

  .error {
    margin: 0;
    font-size: var(--text-sm);
    color: var(--color-danger);
  }

  .labelrow {
    display: inline-flex;
    align-items: baseline;
    gap: 0.15rem;
  }

  .req {
    color: var(--color-text-muted);
  }
</style>
