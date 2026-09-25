<script lang="ts">
	import { Tooltip } from 'bits-ui';
	import ServerPage from '$lib/components/server/ServerPage.svelte';
	import LaunchButton from '$lib/components/toolbar/LaunchButton.svelte';

	const component = new URLSearchParams(location.search).get('component');
</script>

<div class="relative flex h-screen w-full flex-col overflow-hidden">
	<Tooltip.Provider>
		{#if component === 'launch'}
			<div class="flex justify-end p-4"><LaunchButton /></div>
		{:else}
			<ServerPage />
		{/if}
	</Tooltip.Provider>
</div>
