<!--
  §0.4's badge, and §6.3 gaps **G2** and **G3** in the only shape available today.

  `Dot` is boolean (`on?: boolean`) and `Badge`'s five variants have no "in progress", so:
  the **badge carries the colour**, the **dot carries only live/not-live**, and `Starting`
  and `Stopping` get `info` plus a `Spinner` rather than a colour that lies about them.
  Both gaps are proposed upstream; neither is worked around with a second component.

  §7.4: colour is never the only signal. Every badge carries the word from §0.4's Badge
  column, which is also why the word comes from the server rather than from a map here.
-->
<script lang="ts">
  import { Badge, Dot, Spinner } from '@chrissnell/chonky-ui';
  import { spins, variantFor } from '$lib/copy';
  import type { Schemas } from '$lib/api/client';

  interface Props {
    badge: Schemas['Badge'];
    state: string;
    /** Whether this thing is currently receiving data, which is what the dot means. */
    live?: boolean;
    /** The §0.4 sentence, when there is room for it. */
    sentence?: string;
  }

  let { badge, state, live = false, sentence }: Props = $props();
</script>

<span class="status">
  {#if live}
    <Dot on={true} />
  {/if}
  <Badge variant={variantFor(badge.tone)}>{badge.label}</Badge>
  {#if spins(state)}
    <Spinner size={12} />
  {/if}
  {#if sentence}
    <span class="sentence">{sentence}</span>
  {/if}
</span>

<style>
  .status {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    flex-wrap: wrap;
  }

  .sentence {
    color: var(--color-text-muted);
    font-size: var(--text-sm);
  }
</style>
