<script lang="ts">
	import { Tooltip } from 'bits-ui';
	import ServerPage from '$lib/components/server/ServerPage.svelte';
	import LaunchButton from '$lib/components/toolbar/LaunchButton.svelte';
	import Navbar from '$lib/components/misc/Navbar.svelte';
	import Toasts from '$lib/components/misc/Toasts.svelte';

	const params = new URLSearchParams(location.search);
	const component = params.get('component');
	let showPage = $state(!params.has('nopage'));
	(window as any).showServerPage = () => (showPage = true);
</script>

<div class="relative flex h-screen w-full flex-col overflow-hidden">
	<Tooltip.Provider>
		{#if component === 'launch'}
			<div class="flex justify-end p-4"><LaunchButton /></div>
		{:else if component === 'navbar'}
			<div class="flex h-full">
				<Navbar />
				{#if showPage}
					<div class="flex min-w-0 grow flex-col overflow-hidden"><ServerPage /></div>
				{/if}
			</div>
		{:else}
			<ServerPage />
		{/if}
	</Tooltip.Provider>
	<Toasts />
</div>
