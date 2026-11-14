<script lang="ts">
  import UploadServiceForm from '$lib/fragments/UploadServiceForm.svelte';
  import { destinations } from '$lib/copy';

  const d = destinations.find((x) => x.service === 'wunderground')!;
</script>

<UploadServiceForm
  service="wunderground"
  title={d.label}
  blurb={d.summary}
  minimumIntervalSeconds={60}
/>
