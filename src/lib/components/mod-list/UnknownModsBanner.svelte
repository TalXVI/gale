<script lang="ts">
	import { m } from '$lib/paraglide/messages';
	import type { AlternateBackendInfo, UnknownMod } from '$lib/types';
	import { isSwitchPrompt } from '$lib/util';
	import InfoBox from '../ui/InfoBox.svelte';
	import Button from '../ui/Button.svelte';
	import Dialog from '../ui/Dialog.svelte';
	import ModCardList from '../ui/ModCardList.svelte';

	type Props = {
		mods: UnknownMod[];
		uninstallAll: () => Promise<void>;
		switchSource: (alternative: AlternateBackendInfo) => void;
	};

	let { mods, uninstallAll, switchSource }: Props = $props();

	let dialogOpen = $state(false);
</script>

<InfoBox type="warning">
	<div class="flex items-center justify-between">
		<span>{m.unknownModsBanner_content()}</span>
		<Button color="primary" icon="mdi:info" onclick={() => (dialogOpen = true)}
			>{m.unknownModsBanner_details_content()}</Button
		>
	</div>
</InfoBox>

<Dialog bind:open={dialogOpen} title="Unknown mods">
	<div class="text-primary-600 dark:text-primary-300 mb-3">
		{m.unknownModsBanner_dialog_content_1()}
		<br />
		{m.unknownModsBanner_dialog_content_2()}
	</div>

	<ModCardList {mods}>
		{#snippet cardChildren({ mod })}
			{@const alt = mod.alternative}
			{#if alt && isSwitchPrompt(alt.reason)}
				<div class="text-primary-500 dark:text-primary-400 text-sm">
					{m.unknownModsBanner_unavailable({ site: mod.backend })}
				</div>
				<Button
					color="primary"
					icon="mdi:swap-horizontal"
					class="mt-1"
					onclick={() => switchSource(alt)}
				>
					{m.page_modDetails_switchButton({ site: alt.backend, version: alt.latestVersion })}
				</Button>
			{/if}
		{/snippet}
	</ModCardList>

	<div class="mt-2 flex justify-end gap-2">
		<Button color="primary" onclick={() => (dialogOpen = false)}
			>{m.unknownModsBanner_ignore_content()}</Button
		>
		<Button
			icon="mdi:trash"
			color="primary"
			onclick={async () => {
				await uninstallAll();
				dialogOpen = false;
			}}
		>
			{m.unknownModsBanner_uninstall_content()}
		</Button>
	</div>
</Dialog>
