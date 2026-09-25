<script lang="ts">
	import LargeHeading from '$lib/components/prefs/LargeHeading.svelte';
	import SmallHeading from '$lib/components/prefs/SmallHeading.svelte';
	import TabsMenu from '$lib/components/ui/TabsMenu.svelte';
	import Button from '$lib/components/ui/Button.svelte';
	import LocalServerPanel from './LocalServerPanel.svelte';
	import RemoteStatusPanel from './RemoteStatusPanel.svelte';
	import RemoteDeployPanel from './RemoteDeployPanel.svelte';
	import RemoteConnectionSettings from './RemoteConnectionSettings.svelte';
	import RemoteDeploymentSettings from './RemoteDeploymentSettings.svelte';
	import HostProviderSettings from './HostProviderSettings.svelte';
	import { ServerFormState } from './serverForm.svelte';
	import { RemoteSync } from './remoteSync.svelte';
	import { Tabs } from 'bits-ui';
	import profiles from '$lib/state/profile.svelte';
	import { m } from '$lib/paraglide/messages';

	type Props = {
		form?: ServerFormState;
		sync?: RemoteSync;
	};

	let { form = new ServerFormState(), sync = new RemoteSync(form) }: Props = $props();

	// The active profile owns the settings — switching profiles discards
	// any unsaved edits, drops the old profile's sync state, and reloads
	// credentials from scratch.
	let loadedProfile: number | null | undefined = undefined;
	$effect(() => {
		const id = profiles.activeId;
		if (id === loadedProfile) return;
		loadedProfile = id;
		sync.reset();
		// The status panel remounts once loading finishes and issues the
		// live refresh itself; a still-mounted panel is covered here.
		void form.load().then(() => {
			if (sync.mounted && !sync.loadingStatus && sync.status === null && form.remoteConfigured)
				void sync.loadStatus(true).catch(() => {});
		});
	});
</script>

<div
	data-testid="server-page-scroll"
	class="mx-auto flex w-full max-w-3xl grow flex-col gap-1 overflow-y-auto px-6 pt-2 pb-6"
>
	<LargeHeading>{m.serverPage_title()}</LargeHeading>
	<p class="text-primary-600 dark:text-primary-300">
		{m.serverPage_subtitle()}
	</p>

	{#if form.loadingSettings}
		<div class="text-primary-500 mt-5">{m.dedicatedServerDialog_loading()}</div>
	{:else}
		<fieldset disabled={form.busy || sync.busy} class="contents">
			<TabsMenu
				bind:value={form.form.location}
				options={[
					{ value: 'local', label: m.dedicatedServerDialog_locationLocal() },
					{ value: 'remote', label: m.dedicatedServerDialog_locationRemote() }
				]}
			>
				<Tabs.Content value="local">
					<LocalServerPanel {form} />
				</Tabs.Content>

				<Tabs.Content value="remote">
					{#if form.remoteConfigured}
						<RemoteStatusPanel {form} {sync} />
						<RemoteDeployPanel {form} {sync} />
					{:else}
						<div
							class="border-primary-300 dark:border-primary-600 bg-primary-50 dark:bg-primary-900 mt-4 rounded-lg border p-3 text-sm"
						>
							<p class="text-primary-700 dark:text-primary-300 font-medium">
								{m.serverPage_notSetup()}
							</p>
							<p class="text-primary-600 dark:text-primary-400 mt-1">
								{m.serverPage_notSetupInfo()}
							</p>
						</div>
					{/if}

					<SmallHeading>{m.serverPage_sectionConnection()}</SmallHeading>
					<RemoteConnectionSettings {form} />

					<SmallHeading>{m.serverPage_sectionDeployment()}</SmallHeading>
					<RemoteDeploymentSettings {form} />

					<SmallHeading>{m.serverPage_sectionHostProvider()}</SmallHeading>
					<HostProviderSettings {form} />
				</Tabs.Content>
			</TabsMenu>
		</fieldset>
	{/if}
</div>

{#if form.dirty}
	<div
		class="border-primary-300 dark:border-primary-600 dark:bg-primary-900 bg-primary-100 flex shrink-0 flex-wrap items-center gap-3 border-t px-6 py-3"
	>
		<span class="text-primary-700 dark:text-primary-300 font-medium">
			{m.serverPage_unsavedChanges()}
		</span>
		<div class="ml-auto flex gap-2">
			<Button color="primary" disabled={form.busy} onclick={() => form.discard()}>
				{m.serverPage_discard()}
			</Button>
			<Button
				icon="mdi:content-save"
				disabled={form.busy}
				loading={form.saving}
				onclick={() => form.save()}
			>
				{m.dedicatedServerDialog_save()}
			</Button>
		</div>
	</div>
{/if}
