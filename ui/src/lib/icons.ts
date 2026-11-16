/**
 * §6.3 gap **G10**, answered without a second icon vocabulary.
 *
 * chonky's allowlist is 37 icons and has none of the operational ones an admin tool needs
 * -- no `play`, `database`, `server`, `terminal`, `lock`, `key`, `activity`,
 * `alert-triangle`, `info`. The gap note says to extend the allowlist upstream and
 * explicitly says **not** to import `lucide-svelte` across the UI, because that silently
 * introduces a second icon vocabulary rendered by a different component.
 *
 * So: still one vocabulary and still one renderer. chonky's `Icon` takes an `icon`
 * passthrough prop for exactly this, every extra icon is named **here and nowhere else**,
 * and every call site writes `<Icon icon={icons.database} />` through chonky's component.
 * When G10 lands upstream this file shrinks to re-exports and no call site changes.
 *
 * O-11's answer (2026-11-13) is that all twelve gaps are fair game and G2/G4/G9/G10 go
 * upstream now. Upstreaming is a change to `chrissnell/chonky`, not to this repository,
 * so it is a separate pull request against that project; this file is what the UI does in
 * the meantime, and it is deliberately the only such file.
 */
import {
  Activity,
  AlertTriangle,
  Database,
  HardDrive,
  Info,
  Key,
  Link as LinkIcon,
  Lock,
  Pause,
  Play,
  Server,
  Square,
  Terminal,
  Thermometer,
  CloudUpload
} from 'lucide-svelte';

export const icons = {
  activity: Activity,
  alertTriangle: AlertTriangle,
  cloudUpload: CloudUpload,
  database: Database,
  hardDrive: HardDrive,
  info: Info,
  key: Key,
  link: LinkIcon,
  lock: Lock,
  pause: Pause,
  play: Play,
  server: Server,
  square: Square,
  terminal: Terminal,
  thermometer: Thermometer
} as const;

export type ExtraIcon = keyof typeof icons;
