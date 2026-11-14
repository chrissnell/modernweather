import type { PageCopy } from './types';

/**
 * §0.6: six top-level items, organised by what an operator wants rather than by what the
 * daemon contains. "Home" is the addition and it is load-bearing -- §2 is about making
 * "is this actually working?" answerable, and that question needs a place to be answered
 * on arrival.
 */
export interface NavItem {
  readonly href: string;
  readonly label: string;
  readonly summary: string;
  /** Which §2.3 `display.section` value belongs to this item. */
  readonly section: string;
}

export const nav: readonly NavItem[] = [
  { href: '/', label: 'Home', summary: 'Everything at a glance', section: 'home' },
  {
    href: '/stations',
    label: 'My weather stations',
    summary: 'The hardware',
    section: 'stations'
  },
  {
    href: '/share',
    label: 'Share my data',
    summary: 'Weather Underground, PWS Weather, APRS/CWOP, forecasts and alerts',
    section: 'share'
  },
  {
    href: '/website',
    label: 'My website',
    summary: 'The public site and its settings',
    section: 'website'
  },
  {
    href: '/data-storage',
    label: 'Data storage',
    summary: 'The weather database, and sending and receiving between RemoteWeathers',
    section: 'storage'
  },
  {
    href: '/system',
    label: 'System',
    summary: 'Activity, diagnostics, this machine, administrator, advanced',
    section: 'system'
  }
];

export const appName = 'RemoteWeather';

export const common = {
  save: 'Save',
  saving: 'Saving…',
  cancel: 'Cancel',
  close: 'Close',
  back: 'Back',
  retry: 'Retry',
  details: 'Details',
  more: 'More',
  less: 'Less',
  delete: 'Delete',
  remove: 'Remove',
  add: 'Add',
  copy: 'Copy',
  copied: 'Copied',
  download: 'Download',
  test: 'Test connection',
  testing: 'Testing…',
  runNow: 'Run now',
  restart: 'Restart',
  signIn: 'Sign in',
  signOut: 'Sign out',
  skipForNow: 'Skip for now',
  loading: 'Loading…',
  never: 'Never',
  unknown: 'Unknown',
  none: 'None',
  yes: 'Yes',
  no: 'No',
  on: 'On',
  off: 'Off',
  live: 'live',
  reconnecting: 'reconnecting',
  disconnected: 'disconnected',
  offline: "Can't reach RemoteWeather.",
  offlineHint:
    'The page is showing what it last knew. It will catch up on its own when the connection comes back.',
  lastUpdated: (when: string) => `last updated ${when}`,
  notAvailableYet: 'Not available in this version.',
  themeLight: 'Light',
  themeDark: 'Dark',
  // Not "device": §0.2 bans the word because the current UI alternates it with "station"
  // within one screen, and the lint is what stops that recurring. Here it would have meant
  // the operator's laptop rather than their weather station -- a real collision, and the
  // answer is a different word rather than an exception, because an exception outside
  // `advanced.ts` is the hole the rule has no way to close.
  followSystem: 'Match my system'
} as const;

export const pages: Record<string, PageCopy> = {
  home: {
    title: 'Home',
    blurb: 'What your weather station is doing right now.'
  }
};
