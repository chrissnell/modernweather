# Inconsolata, self-hosted

`chonky`'s `tokens.css` names `'Inconsolata'` as `--font-mono` and the package ships no
font (W5 §6.3 gap **G9**). A weather station on a LAN with no internet would otherwise
fall back to Courier New and the whole monospace design language would degrade, so the
font is embedded in the binary like everything else — §6.5's "No CDN" prohibition.

Two weights, `woff2` only, Latin subset: **35.6 KB** against the 45 KB font budget. The
`woff` fallbacks `@fontsource`'s own stylesheet also references are deliberately not
shipped — they are ~22 KB each and every browser that can run Svelte 5 has had `woff2`
support for a decade.

Sourced from `@fontsource/inconsolata`, which is a build dependency rather than a runtime
one: the two files are committed so a release build needs no network. Licence: SIL Open
Font License 1.1, in `LICENSE`.
