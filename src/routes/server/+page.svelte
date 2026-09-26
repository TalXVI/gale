<script lang="ts">
	import ServerPage from '$lib/components/server/ServerPage.svelte';
	import { ServerFormState } from '$lib/components/server/serverForm.svelte';
	import { RemoteSync } from '$lib/components/server/remoteSync.svelte';
	import { goto, beforeNavigate } from '$app/navigation';
	import games from '$lib/state/game.svelte';
	import { m } from '$lib/paraglide/messages';
	import { confirm } from '@tauri-apps/plugin-dialog';

	const form = new ServerFormState();
	const sync = new RemoteSync(form);

	$effect(() => {
		if (games.active && !games.active.dedicatedServer) {
			void goto('/');
		}
	});

	beforeNavigate((navigation) => {
		if (!form.dirty || navigation.to === null) return;
		navigation.cancel();
		void confirm(m.serverPage_unsavedChangesConfirm(), {
			title: m.serverPage_unsavedChanges(),
			kind: 'warning'
		}).then(async (accepted) => {
			if (!accepted || navigation.to === null) return;
			form.discard();
			await goto(navigation.to.url);
		});
	});
</script>

<div class="relative flex w-full flex-col overflow-hidden">
	<ServerPage {form} {sync} />
</div>
