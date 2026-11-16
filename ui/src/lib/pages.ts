/**
 * §7.1's page inventory, as data.
 *
 * It is here rather than implied by the `src/routes` tree for two reasons: the `Command`
 * palette needs a list of destinations with operator-facing names, and the inventory is a
 * **count the design states** -- twenty-six pages -- so a test can assert that the tree
 * and the inventory agree instead of a reviewer counting directories.
 *
 * **The inventory is twenty-seven, not twenty-six.** implementation-plan.md M7-3 says
 * "Twenty-six pages"; §7.1's own table has twenty-seven rows, and all twenty-seven exist
 * in `src/routes`. This is a counting correction rather than a finding of substance --
 * exactly the kind W5 §"The measured route count is 47, not 46" made about its own prompt
 * -- and it is stated so that "every page in the inventory is built" cannot be satisfied
 * while one is quietly missed.
 *
 * `:param` segments are the two parameterised families. They are excluded from the
 * palette because "Station — Settings" is not a destination until there is a station.
 */
export interface PageEntry {
  readonly route: string;
  readonly label: string;
  /** Absent for a parameterised route, which has no fixed destination. */
  readonly href?: string;
}

export const inventory: readonly PageEntry[] = [
  { route: '/', label: 'Home', href: '/' },
  { route: '/stations', label: 'My weather stations', href: '/stations' },
  { route: '/stations/new', label: 'Add a station', href: '/stations/new' },
  { route: '/stations/[key]', label: 'Station — Settings' },
  { route: '/stations/[key]/status', label: 'Station — Status' },
  { route: '/stations/[key]/diagnostics', label: 'Station — Diagnostics' },
  { route: '/share', label: 'Share my data', href: '/share' },
  {
    route: '/share/weather-underground',
    label: 'Weather Underground',
    href: '/share/weather-underground'
  },
  { route: '/share/pws-weather', label: 'PWS Weather', href: '/share/pws-weather' },
  { route: '/share/aprs', label: 'APRS / CWOP', href: '/share/aprs' },
  { route: '/share/forecasts', label: 'Forecasts & alerts', href: '/share/forecasts' },
  { route: '/website', label: 'My website', href: '/website' },
  { route: '/website/[id]', label: 'Website — Settings' },
  { route: '/website/[id]/features', label: 'Website — Extras' },
  { route: '/website/[id]/certificate', label: 'Website — Security certificate' },
  { route: '/data-storage', label: 'Data storage', href: '/data-storage' },
  { route: '/data-storage/database', label: 'Weather database', href: '/data-storage/database' },
  {
    route: '/data-storage/send',
    label: 'Send my readings to another RemoteWeather',
    href: '/data-storage/send'
  },
  {
    route: '/data-storage/receive',
    label: 'Receive readings from another RemoteWeather',
    href: '/data-storage/receive'
  },
  { route: '/system', label: 'System', href: '/system' },
  { route: '/system/activity', label: 'Activity', href: '/system/activity' },
  { route: '/system/diagnostics', label: 'Diagnostics', href: '/system/diagnostics' },
  { route: '/system/machine', label: 'This machine', href: '/system/machine' },
  { route: '/system/administrator', label: 'Administrator', href: '/system/administrator' },
  { route: '/system/advanced', label: 'Advanced', href: '/system/advanced' },
  { route: '/setup', label: 'First-run claim', href: '/setup' },
  { route: '/sign-in', label: 'Sign in', href: '/sign-in' }
];

/** Everything the palette can navigate to: a fixed `href` and a name an operator reads. */
export const allPages = inventory.filter(
  (p): p is PageEntry & { href: string } =>
    typeof p.href === 'string' && p.route !== '/setup' && p.route !== '/sign-in'
);
