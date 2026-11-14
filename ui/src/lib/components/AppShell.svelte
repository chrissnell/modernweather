<!--
  §6.3 gap **G1**: chonky has no app-shell or sidebar primitive, and the owner's own docs
  site hand-rolls its sidebar in `+layout.svelte`. Built from `Box`, `Separator`,
  `ScrollArea` and `Drawer`, following that structure, and worth promoting upstream as
  `AppShell` -- which is not this project's call to make.

  Three things this owns besides layout, each of them a §5 or §7.4 requirement:

  * the **one** live-connection indicator (§5.2) -- `live` / `reconnecting (3s)` /
    `disconnected [Retry]`, never a page that silently shows stale data as current;
  * the mobile `Drawer` and a 44px minimum touch target (§7.4), because checking a station
    from the garden is the real case;
  * the `Command` palette (§6.2), which is how an admin tool with twenty-six pages stays
    navigable by keyboard.
-->
<script lang="ts">
  import {
    Button,
    Command,
    Dot,
    Drawer,
    Icon,
    ScrollArea,
    Separator,
    StatusBar,
    ThemeToggle,
    Toaster
  } from '@chrissnell/chonky-ui';
  import type { Snippet } from 'svelte';
  import { page } from '$app/state';
  import { appName, common, nav, statusCopy } from '$lib/copy';
  import { stream } from '$lib/sse/stream.svelte';
  import { secondsUntil } from '$lib/sse/machine';
  import { allPages } from '$lib/pages';

  interface Props {
    children: Snippet;
    /** The reconciliation banner (§2.4), hoisted here so every page shows it. */
    banner?: Snippet;
  }

  let { children, banner }: Props = $props();

  let drawerOpen = $state(false);
  let paletteOpen = $state(false);

  /**
   * The theme, and the three states it actually has.
   *
   * chonky's `tokens.css` tracks the device on its own -- `:root` is light,
   * `[data-theme="dark"]` is dark, and `@media (prefers-color-scheme: dark)
   * :root:not([data-theme="light"])` is the device -- so **"no explicit choice" is a
   * state**, not a synonym for one of the other two. Collapsing it to "dark" is how this
   * shipped looking wrong on a light laptop with no way back.
   *
   * `theme` is chonky's own storage key. The first version used a key of ours, so the
   * toggle wrote one key and the boot script read another: the choice was lost on every
   * reload and `ThemeToggle`'s own mount-time initialiser re-pinned whatever it liked.
   */
  const STORAGE_KEY = 'theme';

  function stored(): 'dark' | 'light' | null {
    try {
      const value = localStorage.getItem(STORAGE_KEY);
      return value === 'dark' || value === 'light' ? value : null;
    } catch {
      return null;
    }
  }

  function device(): 'dark' | 'light' {
    return window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
  }

  /** The explicit choice, or `null` while the device decides. */
  let chosen = $state<'dark' | 'light' | null>(stored());
  /** What is actually on screen. */
  let theme = $state<'dark' | 'light'>(stored() ?? device());

  function apply(next: 'dark' | 'light') {
    theme = next;
    document.documentElement.setAttribute('data-theme', next);
  }

  function setTheme(next: 'dark' | 'light') {
    chosen = next;
    try {
      localStorage.setItem(STORAGE_KEY, next);
    } catch {
      // A browser with storage denied still gets the theme for this visit.
    }
    apply(next);
  }

  /** Back to tracking the device, which is otherwise a one-way door. */
  function followDevice() {
    chosen = null;
    try {
      localStorage.removeItem(STORAGE_KEY);
    } catch {
      // Nothing to remove.
    }
    apply(device());
  }

  $effect(() => {
    // **`ThemeToggle` writes `data-theme` on mount from whatever it was passed**, which
    // pins the attribute even when nobody has chosen -- so the media query that would have
    // tracked the device never applies again. Listening here is what puts the tracking
    // back: while there is no explicit choice, an OS change is followed live, without a
    // reload. Reported upstream as gap G17.
    const media = window.matchMedia('(prefers-color-scheme: dark)');
    const follow = () => {
      if (chosen === null) apply(device());
    };
    follow();
    media.addEventListener('change', follow);
    return () => media.removeEventListener('change', follow);
  });

  function onkeydown(e: KeyboardEvent) {
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
      e.preventDefault();
      paletteOpen = !paletteOpen;
    }
  }

  const retrySeconds = $derived(secondsUntil(stream.state.retryAt, stream.now));
  const activeSection = $derived.by(() => {
    const path = page.url.pathname;
    const match = nav
      .filter((n) => n.href !== '/' && path.startsWith(n.href))
      .sort((a, b) => b.href.length - a.href.length)[0];
    return match?.href ?? '/';
  });
</script>

<svelte:window {onkeydown} />

<div class="shell">
  <aside>
    <div class="brand">
      <a href="/">{appName}</a>
    </div>
    <ScrollArea.Root class="navscroll">
      <ScrollArea.Viewport>
        <nav aria-label="Sections">
          <ul>
            {#each nav as item (item.href)}
              <li>
                <a
                  href={item.href}
                  aria-current={activeSection === item.href ? 'page' : undefined}
                  class:active={activeSection === item.href}
                >
                  <span class="label">{item.label}</span>
                  <span class="summary">{item.summary}</span>
                </a>
              </li>
            {/each}
          </ul>
        </nav>
      </ScrollArea.Viewport>
    </ScrollArea.Root>
    <Separator />
    <div class="asidefoot">
      <ThemeToggle {theme} onThemeChange={setTheme} />
      <!--
        Without this the toggle is a one-way door: chonky's control is binary, so the first
        click replaces "follow my device" with a fixed choice and there is no way back to
        it. A device that switches theme at sunset is the ordinary case, not an exotic one.
      -->
      {#if chosen !== null}
        <button class="btn btn-sm btn-ghost" type="button" onclick={followDevice}>
          {common.followSystem}
        </button>
      {/if}
    </div>
  </aside>

  <div class="main">
    <header>
      <Button
        class="menu"
        variant="ghost"
        size="sm"
        onclick={() => (drawerOpen = true)}
        aria-label="Sections"
      >
        <Icon name="more-vertical" size="sm" />
      </Button>

      <StatusBar>
        <span class="stream" aria-live="polite">
          {#if stream.state.phase === 'live'}
            <Dot on={true} /> {statusCopy.stream.live}
          {:else if stream.state.phase === 'backfilling' || stream.state.phase === 'connecting'}
            <Dot on={false} /> {statusCopy.stream.live}
          {:else if stream.state.phase === 'reconnecting'}
            <Dot on={false} /> {statusCopy.stream.reconnecting(retrySeconds)}
          {:else}
            <Dot on={false} /> {statusCopy.stream.disconnected}
            <Button variant="ghost" size="sm" onclick={() => stream.retryNow()}>
              {statusCopy.stream.retry}
            </Button>
          {/if}
        </span>
      </StatusBar>

      <Button variant="ghost" size="sm" onclick={() => (paletteOpen = true)}>
        <Icon name="search" size="sm" /> Go to…
      </Button>
    </header>

    {#if banner}
      <div class="banner">{@render banner()}</div>
    {/if}

    <main>{@render children()}</main>
  </div>
</div>

<Drawer.Root bind:open={drawerOpen}>
  <Drawer.Header>{appName}</Drawer.Header>
  <Drawer.Body>
    <nav aria-label="Sections">
      <ul class="drawernav">
        {#each nav as item (item.href)}
          <li>
            <a href={item.href} onclick={() => (drawerOpen = false)}>{item.label}</a>
          </li>
        {/each}
      </ul>
    </nav>
  </Drawer.Body>
</Drawer.Root>

<Command.Dialog bind:open={paletteOpen}>
  <Command.Root>
    <Command.Input placeholder="Go to a page…" />
    <Command.List>
      <Command.Empty>No page by that name.</Command.Empty>
      {#each allPages as item (item.href)}
        <Command.Item
          onSelect={() => {
            paletteOpen = false;
            location.assign(item.href);
          }}
        >
          {item.label}
        </Command.Item>
      {/each}
    </Command.List>
  </Command.Root>
</Command.Dialog>

<Toaster />

<style>
  .shell {
    display: grid;
    grid-template-columns: 17rem 1fr;
    min-height: 100vh;
  }

  aside {
    border-inline-end: 1px solid var(--color-border);
    background: var(--color-surface);
    display: flex;
    flex-direction: column;
  }

  .brand {
    padding: 1rem;
    border-block-end: 1px solid var(--color-border);
  }

  .brand a {
    color: var(--color-text);
    text-decoration: none;
    font-weight: 700;
  }

  nav ul,
  .drawernav {
    list-style: none;
    margin: 0;
    padding: 0.5rem;
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
  }

  nav a {
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
    padding: 0.5rem 0.6rem;
    border-radius: var(--radius);
    color: var(--color-text);
    text-decoration: none;
    border: 1px solid transparent;
  }

  nav a:hover {
    background: var(--color-surface-raised);
  }

  nav a.active {
    border-color: var(--color-border);
    background: var(--color-surface-raised);
  }

  .label {
    font-size: var(--text-sm);
  }

  .summary {
    font-size: var(--text-xs);
    color: var(--color-text-muted);
  }

  .asidefoot {
    padding: 0.75rem;
    display: flex;
    align-items: center;
    gap: 0.5rem;
    flex-wrap: wrap;
  }

  .main {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }

  header {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    padding: 0.5rem 1rem;
    border-block-end: 1px solid var(--color-border);
  }

  header :global(.menu) {
    display: none;
  }

  .stream {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    font-size: var(--text-xs);
    color: var(--color-text-muted);
  }

  .banner {
    padding: 0.75rem 1rem 0;
  }

  main {
    padding: 1rem;
    min-width: 0;
  }

  /* §7.4: responsive down to a phone. The sidebar becomes the Drawer. */
  @media (max-width: 860px) {
    .shell {
      grid-template-columns: 1fr;
    }

    aside {
      display: none;
    }

    header :global(.menu) {
      display: inline-flex;
    }
  }
</style>
