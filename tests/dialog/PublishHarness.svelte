<script lang="ts">
	import SyncPublishDialog from '$lib/components/dialogs/SyncPublishDialog.svelte';
	import Toasts from '$lib/components/misc/Toasts.svelte';

	let open = $state(true);
</script>

<SyncPublishDialog bind:open profileId={1} onPublished={() => {}} />
<Toasts />
