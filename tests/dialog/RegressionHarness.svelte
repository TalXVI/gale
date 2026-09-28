<script lang="ts">
	import Dialog from '$lib/components/ui/Dialog.svelte';
	import LaunchButton from '$lib/components/toolbar/LaunchButton.svelte';
	import auth from '$lib/state/auth.svelte';

	const params = new URLSearchParams(location.search);
	const dialogMode = params.get('dialog');
	let open = $state(true);
	let closeCalls = $state(0);
</script>

{#if params.has('authUser')}
	<p data-testid="sync-user">{auth.user?.displayName ?? 'Sign in'}</p>
{:else if dialogMode}
	<Dialog
		bind:open
		title="Test dialog"
		canClose={dialogMode !== 'blocked'}
		confirmClose={dialogMode === 'confirm' ? { message: 'Close this dialog?' } : null}
		onclose={() => closeCalls++}
	>
		<p>Dialog body</p>
	</Dialog>
	<p data-testid="open-state">{open ? 'open' : 'closed'}</p>
	<p data-testid="close-count">{closeCalls}</p>
	<button onclick={() => (open = true)}>Reopen dialog</button>
{:else}
	<LaunchButton />
{/if}
