<script lang="ts">
	import { Tooltip } from 'bits-ui';
	import ServerSyncDialog from '$lib/components/dialogs/ServerSyncDialog.svelte';
	let open = $state(true);
</script>

<button onclick={() => (open = true)}>Reopen sync dialog</button>
<Tooltip.Provider><ServerSyncDialog bind:open /></Tooltip.Provider>
