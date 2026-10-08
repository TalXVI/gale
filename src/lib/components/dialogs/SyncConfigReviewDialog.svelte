<script lang="ts">
	import Dialog from '$lib/components/ui/Dialog.svelte';
	import Button from '$lib/components/ui/Button.svelte';
	import Checkbox from '$lib/components/ui/Checkbox.svelte';
	import Select from '$lib/components/ui/Select.svelte';
	import * as api from '$lib/api';
	import type { SyncConfigReviewState, SyncConfigUpdatePolicy } from '$lib/types';
	import { pushInfoToast } from '$lib/toast';
	import { confirm } from '@tauri-apps/plugin-dialog';
	import { SvelteSet } from 'svelte/reactivity';
	import { m } from '$lib/paraglide/messages';

	type Mode = 'pending' | 'declined' | 'policies';

	type Props = {
		open: boolean;
		profileId: number;
		updates: SyncConfigReviewState;
		mode: Mode;
		onChanged: () => void | Promise<void>;
	};

	let { open = $bindable(), profileId, updates, mode, onChanged }: Props = $props();

	let selected: SvelteSet<string> = $state(new SvelteSet());
	let loading = $state(false);
	const id = $props.id();

	let reasonLabel = $derived({
		modifiedLocally: m.syncConfigReviewDialog_reason_modifiedLocally(),
		deletedLocally: m.syncConfigReviewDialog_reason_deletedLocally(),
		newFile: m.syncConfigReviewDialog_reason_newFile(),
		publishedUpdate: m.syncConfigReviewDialog_reason_publishedUpdate()
	});

	let title = $derived(
		mode === 'declined'
			? m.syncConfigReviewDialog_title_declined()
			: mode === 'policies'
				? m.syncConfigReviewDialog_title_policies()
				: m.syncConfigReviewDialog_title()
	);

	let emptyMessage = $derived(
		mode === 'declined'
			? m.syncConfigReviewDialog_empty_declined()
			: mode === 'policies'
				? m.syncConfigReviewDialog_empty_policies()
				: m.syncConfigReviewDialog_empty()
	);

	let items = $derived(mode === 'policies' ? [] : updates[mode]);

	let policyItems = $derived([
		{ value: 'ask', label: m.syncConfigReviewDialog_policy_ask() },
		{ value: 'alwaysKeep', label: m.syncConfigReviewDialog_policy_alwaysKeep() }
	]);

	$effect(() => {
		mode;
		if (open) {
			selected = new SvelteSet();
		}
	});

	async function applySelected() {
		let files = [...selected];

		let current = await api.profile.sync.getPendingConfig(profileId);
		let list = mode === 'pending' ? current.pending : current.declined;

		if (files.some((path) => !list.some((item) => item.path === path))) {
			selected = new SvelteSet();
			await onChanged();
			return;
		}

		let restoreDeleted = files.filter(
			(path) => list.find((item) => item.path === path)?.reason === 'deletedLocally'
		);

		if (restoreDeleted.length > 0) {
			let confirmed = await confirm(
				m.syncConfigReviewDialog_restoreConfirm({ count: restoreDeleted.length })
			);
			if (!confirmed) return;
		}

		loading = true;
		try {
			await api.profile.sync.applyConfig(files, false, restoreDeleted, profileId);
			pushInfoToast({ message: m.syncConfigReviewDialog_applyMessage() });
			selected = new SvelteSet();
			await onChanged();
		} finally {
			loading = false;
		}
	}

	async function declineSelected() {
		loading = true;
		try {
			await api.profile.sync.declineConfig([...selected], false, profileId);
			pushInfoToast({ message: m.syncConfigReviewDialog_declineMessage() });
			selected = new SvelteSet();
			await onChanged();
		} finally {
			loading = false;
		}
	}

	async function setPolicy(path: string, policy: string) {
		loading = true;
		try {
			await api.profile.sync.setConfigPolicy(path, policy as SyncConfigUpdatePolicy, profileId);
			await onChanged();
		} finally {
			loading = false;
		}
	}
</script>

<Dialog bind:open {title}>
	{#if mode !== 'policies'}
		<p class="text-primary-600 dark:text-primary-300 mt-2 text-sm">
			{m.syncConfigReviewDialog_replaceHint()}
		</p>
	{/if}
	{#if mode === 'policies'}
		{#if updates.policies.length === 0}
			<div class="text-primary-600 dark:text-primary-300 my-4 text-center">
				{emptyMessage}
			</div>
		{:else}
			<div class="mt-3 flex max-h-60 flex-col gap-1 overflow-y-auto pr-1">
				{#each updates.policies as entry (entry.path)}
					<div
						class="dark:bg-primary-900 bg-primary-100 flex items-center gap-2.5 rounded-md px-3 py-1.5"
					>
						<span class="text-primary-800 dark:text-primary-100 truncate" title={entry.path}>
							{entry.path}
						</span>
						<Select
							triggerClass="ml-auto shrink-0"
							items={policyItems}
							type="single"
							value={entry.policy === 'alwaysApply' ? 'ask' : entry.policy}
							onValueChange={(value) => setPolicy(entry.path, value)}
							disabled={loading}
							avoidCollisions={false}
						/>
					</div>
				{/each}
			</div>
		{/if}
	{:else if items.length === 0}
		<div class="text-primary-600 dark:text-primary-300 my-4 text-center">
			{emptyMessage}
		</div>
	{:else}
		<div class="mt-3 flex max-h-60 flex-col gap-1 overflow-y-auto pr-1">
			{#each items as update (update.path)}
				<div
					class="dark:bg-primary-900 bg-primary-100 flex items-center gap-2.5 rounded-md px-3 py-1.5"
				>
					<Checkbox
						id={`${id}-${update.path}`}
						disabled={loading}
						checked={selected.has(update.path)}
						onCheckedChange={(checked) => {
							if (checked) {
								selected.add(update.path);
							} else {
								selected.delete(update.path);
							}
						}}
					/>
					<label
						for={`${id}-${update.path}`}
						class="text-primary-800 dark:text-primary-100 truncate"
						title={update.path}
					>
						{update.path}
					</label>
					<span class="text-primary-500 dark:text-primary-400 shrink-0 text-sm">
						{reasonLabel[update.reason]}
					</span>
				</div>
			{/each}
		</div>

		<div class="mt-4 flex justify-end gap-2">
			{#if mode === 'pending'}
				<Button
					color="primary"
					onclick={declineSelected}
					{loading}
					disabled={selected.size === 0}
					icon="mdi:close"
				>
					{m.syncConfigReviewDialog_decline()}
				</Button>
			{/if}
			<Button onclick={applySelected} {loading} disabled={selected.size === 0} icon="mdi:check">
				{m.syncConfigReviewDialog_apply()}
			</Button>
		</div>
	{/if}
</Dialog>
