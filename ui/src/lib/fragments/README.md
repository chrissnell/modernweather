# Shared page fragments

Not components in §6.3's sense -- they add no vocabulary and no look, and none of them
exists because of a gap in `chonky`. Each is markup that **two or more routes render**,
factored out because the alternative is two copies that drift:

| Fragment | Rendered by | Why it is not duplicated |
|---|---|---|
| `ApplyBanner.svelte` | the root layout, for every page | §2.4's report follows the operator off the page they saved on, because reconciliation takes seconds and they will have navigated |
| `StationForm.svelte` | `/stations/new` and `/stations/[key]` | §0.7 step 2 and §7.1's "Station — Settings" are the same form over the same fields; two copies is how one of them stops asking for a wind-direction correction |
| `UploadServiceForm.svelte` | `/share/weather-underground` and `/share/pws-weather` | §7.1 says PWS Weather is "Same shape" in as many words |

The seven-component inventory in `../components/` is unchanged at seven.
