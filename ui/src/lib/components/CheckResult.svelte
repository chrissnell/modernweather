<!--
  One diagnostic's result: §6.2 maps it to `Box` + `Spinner` -> `Badge` + `Collapsible`.

  The detail is behind a disclosure rather than printed inline because the useful part of
  a failed check is its **first** line and the useful part of a confusing one is all forty
  -- and D-34's cause chain means the forty lines exist.
-->
<script lang="ts">
  import { Badge, Box, Collapsible, Button, Spinner } from '@chrissnell/chonky-ui';
  import { common } from '$lib/copy';

  export type Outcome = 'idle' | 'running' | 'passed' | 'failed' | 'unavailable';

  interface Props {
    name: string;
    description: string;
    outcome: Outcome;
    message?: string;
    detail?: string;
    ran?: string;
    onrun?: () => void;
  }

  let { name, description, outcome, message, detail, ran, onrun }: Props = $props();
  let open = $state(false);
</script>

<Box>
  <div class="head">
    <div>
      <h3>{name}</h3>
      <p class="desc">{description}</p>
    </div>
    <div class="right">
      {#if outcome === 'running'}
        <Spinner size={14} />
        <Badge variant="info">{common.testing}</Badge>
      {:else if outcome === 'passed'}
        <Badge variant="success">Passed</Badge>
      {:else if outcome === 'failed'}
        <Badge variant="danger">Failed</Badge>
      {:else if outcome === 'unavailable'}
        <Badge variant="default">Not available</Badge>
      {/if}
      {#if onrun}
        <Button size="sm" onclick={onrun} disabled={outcome === 'running'}>
          {common.runNow}
        </Button>
      {/if}
    </div>
  </div>

  {#if message}
    <p class="message">{message}</p>
  {/if}
  {#if ran}
    <p class="desc">Last run {ran}.</p>
  {/if}

  {#if detail}
    <Collapsible.Root bind:open>
      <Collapsible.Trigger>{open ? common.less : common.details}</Collapsible.Trigger>
      <Collapsible.Content>
        <pre>{detail}</pre>
      </Collapsible.Content>
    </Collapsible.Root>
  {/if}
</Box>

<style>
  .head {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: 1rem;
    flex-wrap: wrap;
  }

  .right {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }

  h3 {
    margin: 0;
    font-size: var(--text-base);
  }

  .desc {
    margin: 0.15rem 0 0;
    font-size: var(--text-xs);
    color: var(--color-text-muted);
  }

  .message {
    margin: 0.5rem 0 0;
    font-size: var(--text-sm);
  }

  pre {
    margin: 0.5rem 0 0;
    padding: 0.5rem;
    overflow-x: auto;
    font-size: var(--text-xs);
    background: var(--color-surface-raised);
    border: 1px solid var(--color-border-subtle);
    border-radius: var(--radius);
  }
</style>
