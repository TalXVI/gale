<script lang="ts">
	import { Tooltip } from 'bits-ui';
	import ServerSyncDialog from '$lib/components/dialogs/ServerSyncDialog.svelte';
</script>

<Tooltip.Provider><ServerSyncDialog open={true} /></Tooltip.Provider>
