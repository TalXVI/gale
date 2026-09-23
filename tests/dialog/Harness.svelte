<script lang="ts">
	import { Tooltip } from 'bits-ui';
	import ServerSyncDialog from '$lib/components/dialogs/ServerSyncDialog.svelte';
	import DedicatedServerDialog from '$lib/components/dialogs/DedicatedServerDialog.svelte';
	const setup = new URLSearchParams(location.search).has('setup');
	let open = $state(true);
</script>

<button onclick={() => (open = true)}>Reopen sync dialog</button>
<Tooltip.Provider>
	{#if setup}<DedicatedServerDialog bind:open />{:else}<ServerSyncDialog bind:open />{/if}
</Tooltip.Provider>
