<script lang="ts">
	import Dialog from '$lib/components/ui/Dialog.svelte';
	import Button from '$lib/components/ui/Button.svelte';
	import Select from '$lib/components/ui/Select.svelte';
	import Checkbox from '$lib/components/ui/Checkbox.svelte';
	import Label from '$lib/components/ui/Label.svelte';
	import InfoBox from '$lib/components/ui/InfoBox.svelte';
	import InputField from '$lib/components/ui/InputField.svelte';
	import DeploymentStats from './DeploymentStats.svelte';
	import * as api from '$lib/api';
	import type {
		DeploySelection,
		PlanConfigEntry,
		RestartPolicy,
		ServerSyncPreview,
		ServerSyncProgress,
		ServerSyncResult,
		ServerSyncStageProgress,
		ServerSyncStatus,
		SyncConfigUpdatePolicy
	} from '$lib/types';
	import { listen, type UnlistenFn } from '@tauri-apps/api/event';
	import { message } from '@tauri-apps/plugin-dialog';
	import { onDestroy } from 'svelte';
	import { m } from '$lib/paraglide/messages';

	type Props = { open?: boolean };
	let { open = $bindable(false) }: Props = $props();

	type Scope = 'mods' | 'configs' | 'both';
	/// What the user decided for one config path.
	type Decision = 'apply' | 'decline';

	let scope = $state<Scope>('both');
	let decisions = $state<Record<string, Decision>>({});
	let status = $state<ServerSyncStatus | null>(null);
	let preview = $state<ServerSyncPreview | null>(null);
	let result = $state<ServerSyncResult | null>(null);
	/// Whether the selection changed after the last preview — the approved
	/// plan hash only binds the previewed selection, so Deploy requires a
	/// fresh preview first.
	let dirty = $state(true);
	let loadingStatus = $state(false);
	let previewing = $state(false);
	let deploying = $state(false);
	let progress = $state<ServerSyncProgress | null>(null);
	let stageProgress = $state<ServerSyncStageProgress | null>(null);
	let restartPolicy = $state<RestartPolicy>('manual');
	let remotePassword = $state('');
	let workerToken = $state('');
	let workerAutoSync = $state(false);
	let workerAutoMods = $state(false);
	let savingWorker = $state(false);
	let unlisten: UnlistenFn[] = [];

	$effect(() => {
		if (!open) {
			unlisten.forEach((fn) => fn());
			unlisten = [];
			preview = null;
			result = null;
			progress = null;
			stageProgress = null;
			dirty = true;
			remotePassword = '';
			workerToken = '';
			return;
		}
		void loadStatus(false);
		void listenToProgress();
	});

	onDestroy(() => unlisten.forEach((fn) => fn()));

	async function listenToProgress() {
		unlisten = [
			await listen<ServerSyncProgress>('server_sync_progress', (event) => {
				if (deploying) progress = event.payload;
			}),
			await listen<ServerSyncStageProgress>('server_sync_stage_progress', (event) => {
				if (previewing || deploying) stageProgress = event.payload;
			})
		];
	}

	async function loadStatus(refresh: boolean) {
		loadingStatus = true;
		try {
			status = await api.profile.server.getSyncStatus(refresh, remotePassword, workerToken);
			if (status.worker) {
				workerAutoSync = status.worker.autoSync;
				workerAutoMods = status.worker.autoMods;
				restartPolicy = status.worker.restartPolicy;
			}
		} finally {
			loadingStatus = false;
		}
	}

	function isWorker() {
		return status?.mode === 'worker';
	}

	function selection(): DeploySelection {
		const applyConfigs: string[] = [];
		const restoreConfigs: string[] = [];
		const declineConfigs: string[] = [];

		for (const [path, decision] of Object.entries(decisions)) {
			if (decision === 'decline') {
				declineConfigs.push(path);
			} else {
				applyConfigs.push(path);
				// Restoring recreates a file the server side deleted; the
				// planner requires the extra authorization for that case.
				if (pendingReason(path) === 'deletedLocally') restoreConfigs.push(path);
			}
		}

		return {
			includeMods: scope !== 'configs',
			includeConfigs: scope !== 'mods',
			applyConfigs,
			restoreConfigs,
			declineConfigs
		};
	}

	function pendingReason(path: string) {
		const entry = preview?.plan.configEntries.find((entry) => entry.path === path);
		return entry?.action === 'pending' ? entry.reason : null;
	}

	async function previewSync() {
		previewing = true;
		result = null;
		try {
			// The restart policy is bound into the plan hash — the approval is
			// only valid while this selection stands.
			preview = await api.profile.server.previewSync(
				selection(),
				restartPolicy,
				remotePassword,
				workerToken
			);
			dirty = false;
		} finally {
			previewing = false;
			stageProgress = null;
		}
	}

	function decide(path: string, decision: Decision | null) {
		if (decision === null) delete decisions[path];
		else decisions[path] = decision;
		dirty = true;
	}

	/// A persistent per-file policy for *future* revisions — distinct from
	/// the one-time Apply/Decline decision for the current conflict.
	async function setPolicy(path: string, policy: SyncConfigUpdatePolicy) {
		await api.profile.server.setConfigPolicy(path, policy, remotePassword, workerToken);
		dirty = true;
	}

	async function deploy(force = false) {
		if (!preview) return;
		deploying = true;
		progress = null;
		try {
			result = await api.profile.server.deploySync(
				selection(),
				preview.plan.hash,
				restartPolicy,
				force,
				remotePassword,
				workerToken
			);
			decisions = {};
			preview = null;
			dirty = true;
			void loadStatus(false);
		} catch (error) {
			await message(error instanceof Error ? error.message : String(error), {
				title: m.serverSync_deployFailedTitle(),
				kind: 'error'
			});
		} finally {
			deploying = false;
			progress = null;
			stageProgress = null;
		}
	}

	async function saveWorkerConfig() {
		savingWorker = true;
		try {
			await api.profile.server.configureWorker(
				workerAutoSync,
				workerAutoMods,
				restartPolicy,
				workerToken
			);
			await loadStatus(false);
		} finally {
			savingWorker = false;
		}
	}

	function actionLabel(entry: PlanConfigEntry): string {
		switch (entry.action) {
			case 'write':
				return m.serverSync_actionWrite();
			case 'markApplied':
				return m.serverSync_actionApplied();
			case 'keep':
				return m.serverSync_actionKeep();
			case 'decline':
				return m.serverSync_actionDeclined();
			case 'pending':
				return entry.reason === 'deletedLocally'
					? m.syncConfigReviewDialog_reason_deletedLocally()
					: m.syncConfigReviewDialog_reason_modifiedLocally();
			case 'unapplied':
				return m.serverSync_actionUnapplied();
		}
	}

	function restartLabel(outcome: string): string {
		switch (outcome) {
			case 'restarted':
				return m.serverSync_restartDone();
			case 'awaitingManual':
				return m.serverSync_restartLeftManual();
			case 'awaitingEmpty':
				return m.serverSync_restartAwaitingEmpty();
			case 'startupUnverified':
				return m.serverSync_restartUnverified();
			case 'failed':
				return m.serverSync_restartFailed();
			default:
				return outcome;
		}
	}
</script>

<Dialog title={m.serverSync_title()} bind:open large>
	<p class="text-primary-600 dark:text-primary-300 mt-1">{m.serverSync_content()}</p>

	{#if status}
		<div
			class="border-primary-300 dark:border-primary-600 bg-primary-50 dark:bg-primary-900 mt-4 rounded-lg border p-3 text-sm"
		>
			<div class="flex items-center justify-between gap-3">
				<span class="text-primary-700 dark:text-primary-300 font-medium">
					{isWorker() ? m.serverSync_modeWorker() : m.serverSync_modeLocal()}
				</span>
				<Button loading={loadingStatus} onclick={() => loadStatus(true)}>
					{m.serverSync_refresh()}
				</Button>
			</div>
			<div class="text-primary-600 dark:text-primary-400 mt-2 flex flex-col gap-1">
				<span>
					{m.serverSync_publication({
						revision: status.publicationRevision
							? new Date(status.publicationRevision).toLocaleString()
							: m.serverSync_noPublication()
					})}
				</span>
				{#if status.server}
					<span>
						{m.serverSync_deployed({
							revision: status.server.modsRevision ?? m.serverSync_neverDeployed()
						})}
					</span>
					{#if status.server.restartRequired}
						<span class="text-orange-600 dark:text-orange-400"
							>{m.serverSync_restartRequired()}</span
						>
					{/if}
					{#if status.server.pendingConfigs > 0}
						<span>{m.serverSync_pendingCount({ count: status.server.pendingConfigs })}</span>
					{/if}
					{#if status.server.lease}
						<span class="text-orange-600 dark:text-orange-400">
							{m.serverSync_leaseHeld({ owner: status.server.lease.owner })}
						</span>
					{/if}
				{/if}
				{#if status.worker?.busy}
					<span class="text-orange-600 dark:text-orange-400">
						{m.serverSync_workerBusy()}
					</span>
				{/if}
				{#if status.worker?.pendingRevision}
					<span class="text-orange-600 dark:text-orange-400">
						{m.serverSync_pendingRevision({
							revision: new Date(status.worker.pendingRevision).toLocaleString()
						})}
					</span>
				{/if}
				{#if status.worker?.lastError}
					<span class="text-red-600 dark:text-red-400">{status.worker.lastError}</span>
				{/if}
				{#each status.warnings as warning}
					<span class="text-orange-600 dark:text-orange-400">{warning}</span>
				{/each}
				{#if status.credentialRequired}
					<span class="text-red-600 dark:text-red-400">{m.serverSync_credentialRequired()}</span>
				{/if}
			</div>
		</div>

		{#if isWorker() && status.worker}
			<details class="mt-3">
				<summary class="text-primary-600 dark:text-primary-300 cursor-pointer">
					{m.serverSync_workerAutomation()}
				</summary>
				<div class="mt-2 flex flex-col gap-3">
					<div class="flex items-center">
						<Label>{m.serverSync_autoSync()}</Label>
						<Checkbox bind:checked={workerAutoSync} />
					</div>
					<div class="flex items-center">
						<Label>{m.serverSync_autoMods()}</Label>
						<Checkbox bind:checked={workerAutoMods} />
					</div>
					<Button color="primary" loading={savingWorker} onclick={saveWorkerConfig}>
						{m.serverSync_saveAutomation()}
					</Button>
				</div>
			</details>
		{/if}
	{/if}

	<div class="mt-4">
		<Label>{m.serverSync_scope()}</Label>
		<Select
			type="single"
			triggerClass="mt-1 w-full"
			bind:value={scope}
			onValueChange={() => (dirty = true)}
			items={[
				{ value: 'both', label: m.serverSync_scopeBoth() },
				{ value: 'mods', label: m.serverSync_scopeMods() },
				{ value: 'configs', label: m.serverSync_scopeConfigs() }
			]}
		/>
	</div>

	<details class="mt-3">
		<summary class="text-primary-600 dark:text-primary-300 cursor-pointer">
			{m.serverSync_credentials()}
		</summary>
		<div class="mt-2 flex flex-col gap-3">
			{#if isWorker()}
				<div>
					<Label>{m.serverSync_workerToken()}</Label>
					<InputField class="mt-1 w-full" bind:value={workerToken} type="password" />
					<p class="text-primary-500 mt-1 text-sm">{m.serverSync_savedCredential()}</p>
				</div>
			{:else}
				<div>
					<Label>{m.dedicatedServerDialog_password()}</Label>
					<InputField class="mt-1 w-full" bind:value={remotePassword} type="password" />
					<p class="text-primary-500 mt-1 text-sm">{m.serverSync_savedCredential()}</p>
				</div>
			{/if}
		</div>
	</details>

	{#if stageProgress}
		<p class="text-primary-500 mt-3 text-sm">
			{m.serverSync_staging({
				mod: stageProgress.mod,
				completed: stageProgress.completed,
				total: stageProgress.total
			})}
		</p>
	{/if}

	{#if preview}
		<div class="mt-4">
			<DeploymentStats
				uploaded={preview.plan.uploads.length}
				bytes={preview.plan.uploadBytes}
				removed={preview.plan.removals.length}
				unchanged={preview.plan.unchangedFiles}
			/>

			{#if preview.busy}
				<InfoBox type="warning" class="mt-3">
					{m.serverSync_leaseHeld({ owner: preview.busy.record.owner })}
				</InfoBox>
			{/if}
			{#each preview.warnings as warning}
				<InfoBox type="warning" class="mt-3">{warning}</InfoBox>
			{/each}
			{#if preview.plan.unmanaged.length > 0}
				<InfoBox type="info" class="mt-3">
					{m.serverSync_unmanaged({ count: preview.plan.unmanaged.length })}
					<details class="mt-1">
						{#each preview.plan.unmanaged as path}
							<div class="font-mono wrap-anywhere">{path}</div>
						{/each}
					</details>
				</InfoBox>
			{/if}
			{#if preview.plan.requiresRestart}
				<InfoBox type="info" class="mt-3">{m.serverSync_willRestart()}</InfoBox>
			{/if}

			{#if preview.plan.configEntries.length > 0}
				<details class="mt-3" open={preview.plan.conflicts.length > 0}>
					<summary class="text-primary-700 dark:text-primary-300 cursor-pointer font-medium">
						{m.serverSync_configFiles({ count: preview.plan.configEntries.length })}
					</summary>
					<div
						class="border-primary-300 dark:border-primary-600 bg-primary-50 dark:bg-primary-900 mt-2 max-h-64 overflow-auto rounded-lg border p-3 text-sm"
					>
						{#each preview.plan.configEntries as entry}
							<div class="flex items-center gap-2 py-1">
								<span class="text-primary-700 dark:text-primary-300 grow font-mono wrap-anywhere">
									{entry.path}
								</span>
								<span class="text-primary-500 shrink-0">{actionLabel(entry)}</span>
								<Select
									type="single"
									triggerClass="w-44 shrink-0"
									value={entry.policy}
									onValueChange={(value) => setPolicy(entry.path, value as SyncConfigUpdatePolicy)}
									items={[
										{ value: 'ask', label: m.serverSync_policyAsk() },
										{ value: 'alwaysApply', label: m.serverSync_policyAlwaysApply() },
										{ value: 'alwaysKeep', label: m.serverSync_policyAlwaysKeep() }
									]}
								/>
								{#if entry.action === 'pending'}
									<Button onclick={() => decide(entry.path, 'apply')}>
										{entry.reason === 'deletedLocally'
											? m.serverSync_restore()
											: m.serverSync_apply()}
									</Button>
									<Button onclick={() => decide(entry.path, 'decline')}>
										{m.serverSync_decline()}
									</Button>
								{:else if decisions[entry.path]}
									<span class="shrink-0 text-green-600 dark:text-green-400">
										{decisions[entry.path] === 'apply'
											? m.serverSync_willApply()
											: m.serverSync_willDecline()}
									</span>
									<Button onclick={() => decide(entry.path, null)}>
										{m.serverSync_undo()}
									</Button>
								{/if}
							</div>
						{/each}
					</div>
				</details>
			{/if}

			{#if preview.plan.uploads.length > 0 || preview.plan.removals.length > 0}
				<details class="mt-3">
					<summary class="text-primary-700 dark:text-primary-300 cursor-pointer font-medium">
						{m.deploymentPreview_fileChanges({
							count: preview.plan.uploads.length + preview.plan.removals.length
						})}
					</summary>
					<div
						class="border-primary-300 dark:border-primary-600 bg-primary-50 dark:bg-primary-900 mt-2 max-h-64 overflow-auto rounded-lg border p-3 font-mono text-sm"
					>
						{#if preview.plan.uploads.length > 0}
							<div class="text-primary-500 mb-1 font-sans font-semibold">
								{m.deploymentPreview_uploads()}
							</div>
							{#each preview.plan.uploads as upload}
								<div class="text-primary-700 dark:text-primary-300 flex gap-2 py-0.5">
									<span class="shrink-0 text-green-600 dark:text-green-400">+</span>
									<span class="wrap-anywhere">{upload.path}</span>
								</div>
							{/each}
						{/if}
						{#if preview.plan.removals.length > 0}
							<div class="text-primary-500 mt-3 mb-1 font-sans font-semibold">
								{m.deploymentPreview_removals()}
							</div>
							{#each preview.plan.removals as path}
								<div class="text-primary-700 dark:text-primary-300 flex gap-2 py-0.5">
									<span class="shrink-0 text-red-600 dark:text-red-400">−</span>
									<span class="wrap-anywhere">{path}</span>
								</div>
							{/each}
						{/if}
					</div>
				</details>
			{/if}
		</div>
	{/if}

	{#if deploying && progress}
		<div class="mt-4 flex flex-col gap-1">
			<div class="text-primary-600 dark:text-primary-300 flex justify-between gap-3 text-sm">
				<span class="truncate">{progress.path}</span>
				<span class="shrink-0">{progress.completed}/{progress.total}</span>
			</div>
		</div>
	{/if}

	{#if result}
		<div class="mt-4">
			{#if result.state.lastOperation?.status === 'partial' || result.failedConfigWrites.length > 0}
				<InfoBox type="warning">{m.serverSync_partialDone()}</InfoBox>
			{:else}
				<InfoBox type="info">{m.serverSync_deployed_done()}</InfoBox>
			{/if}
			<DeploymentStats
				uploaded={result.summary.uploadedFiles}
				bytes={result.summary.uploadedBytes}
				removed={result.summary.removedFiles}
				unchanged={result.summary.unchangedFiles}
			/>
			<p class="text-primary-600 dark:text-primary-300 mt-2 text-sm">
				{restartLabel(result.restart)}
			</p>
			{#if result.failedConfigWrites.length > 0}
				<InfoBox type="warning" class="mt-2">
					{m.serverSync_failedConfigs({ count: result.failedConfigWrites.length })}
				</InfoBox>
			{/if}
			{#each result.warnings as warning}
				<InfoBox type="warning" class="mt-2">{warning}</InfoBox>
			{/each}
		</div>
	{/if}

	<div class="mt-5 flex w-full items-center justify-end gap-2">
		<Button color="primary" onclick={() => (open = false)}>{m.serverSync_close()}</Button>
		{#if dirty}
			<span class="text-primary-500 mr-auto text-sm">{m.serverSync_dirtyHint()}</span>
		{/if}
		<div class="flex items-center gap-2">
			<Label>{m.serverSync_restartPolicy()}</Label>
			<Select
				type="single"
				triggerClass="w-40"
				bind:value={restartPolicy}
				onValueChange={() => (dirty = true)}
				items={[
					{ value: 'manual', label: m.serverSync_restartManual() },
					{ value: 'immediate', label: m.serverSync_restartImmediate() },
					{ value: 'whenEmpty', label: m.serverSync_restartWhenEmpty() }
				]}
			/>
		</div>
		<Button icon="mdi:cloud-search" loading={previewing} onclick={previewSync}>
			{m.serverSync_preview()}
		</Button>
		{#if preview?.busy?.stale}
			<Button
				icon="mdi:cloud-upload"
				loading={deploying}
				disabled={dirty}
				onclick={() => deploy(true)}
			>
				{m.serverSync_takeover()}
			</Button>
		{:else}
			<Button
				icon="mdi:cloud-upload"
				loading={deploying}
				disabled={!preview || dirty || !!preview.busy}
				onclick={() => deploy()}
			>
				{m.serverSync_deploy()}
			</Button>
		{/if}
	</div>
</Dialog>
