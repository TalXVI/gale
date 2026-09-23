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
		DeployScope,
		PlanConfigEntry,
		RestartPolicy,
		ServerSyncPreview,
		ServerSyncProgress,
		ServerSyncResult,
		ServerSyncStageProgress,
		ServerSyncStatus,
		SyncConfigUpdatePolicy,
		WorkerStatus
	} from '$lib/types';
	import { listen } from '@tauri-apps/api/event';
	import { confirm, message } from '@tauri-apps/plugin-dialog';
	import { untrack } from 'svelte';
	import { m } from '$lib/paraglide/messages';

	type Props = { open?: boolean };
	let { open = $bindable(false) }: Props = $props();
	const formId = $props.id();

	/// What the user decided for one config path.
	type Decision = 'apply' | 'restore' | 'decline';

	let scope = $state<DeployScope>('both');
	let decisions = $state<Record<string, Decision>>({});
	let status = $state<ServerSyncStatus | null>(null);
	let preview = $state<ServerSyncPreview | null>(null);
	let result = $state<ServerSyncResult | null>(null);
	let restartPolicy = $state<RestartPolicy>('manual');
	let approvedInput = $state('');
	const currentInput = $derived(JSON.stringify({ selection: selection(), restartPolicy }));
	const dirty = $derived(!preview || approvedInput !== currentInput);
	let loadingStatus = $state(false);
	let previewing = $state(false);
	let deploying = $state(false);
	let progress = $state<ServerSyncProgress | null>(null);
	let stageProgress = $state<ServerSyncStageProgress | null>(null);
	let remotePassword = $state('');
	let workerToken = $state('');
	let workerAutoSync = $state(false);
	let workerAutoMods = $state(false);
	let workerRestartPolicy = $state<RestartPolicy>('manual');
	let savingWorker = $state(false);
	let savingPolicy = $state(false);
	/// Policy picks still being confirmed by the backend. While a save is in
	/// flight the override is what the dropdown shows; once it lands the value
	/// is folded into the preview entry, and on failure the override is dropped
	/// so the control falls back to the last confirmed policy.
	let policyOverrides = $state<Record<string, SyncConfigUpdatePolicy>>({});
	let acknowledgingRestart = $state(false);
	let reviewOnly = $state(false);
	let loadingPreferences = $state(false);
	let savingPreferences = $state(false);
	const busy = $derived(
		previewing ||
			deploying ||
			savingWorker ||
			savingPolicy ||
			savingPreferences ||
			acknowledgingRestart
	);
	const remainingDecisions = $derived(
		preview?.plan.configEntries.filter(
			(entry) => entry.action === 'pending' && !decisions[entry.path]
		).length ?? 0
	);
	const visibleConfigEntries = $derived.by(() => {
		const entries = preview?.plan.configEntries ?? [];
		return entries
			.filter((entry) => !reviewOnly || entry.action === 'pending')
			.sort((a, b) => Number(b.action === 'pending') - Number(a.action === 'pending'));
	});

	$effect(() => {
		if (!open) {
			decisions = {};
			reviewOnly = false;
			preview = null;
			policyOverrides = {};
			result = null;
			progress = null;
			stageProgress = null;
			approvedInput = '';
			remotePassword = '';
			workerToken = '';
			return;
		}
		untrack(() => {
			void loadPreferences();
			void loadStatus(false);
		});
		const listeners = Promise.all([
			listen<ServerSyncProgress>('server_sync_progress', (event) => {
				if (deploying) progress = event.payload;
			}),
			listen<ServerSyncStageProgress>('server_sync_stage_progress', (event) => {
				if (previewing || deploying) stageProgress = event.payload;
			})
		]);
		return () => void listeners.then((unlisten) => unlisten.forEach((fn) => fn()));
	});

	async function loadStatus(refresh: boolean) {
		loadingStatus = true;
		try {
			status = await api.profile.server.getSyncStatus(refresh, remotePassword, workerToken);
			if (status.worker) {
				workerAutoSync = status.worker.autoSync;
				workerAutoMods = status.worker.autoMods;
				workerRestartPolicy = status.worker.restartPolicy;
			}
		} finally {
			loadingStatus = false;
		}
	}

	async function loadPreferences() {
		loadingPreferences = true;
		try {
			const preferences = await api.profile.server.getSyncDialogPreferences();
			scope = preferences.scope;
			restartPolicy = preferences.restartPolicy;
		} finally {
			loadingPreferences = false;
		}
	}

	async function savePreferences(nextScope: DeployScope, nextRestartPolicy: RestartPolicy) {
		const previous = { scope, restartPolicy };
		scope = nextScope;
		restartPolicy = nextRestartPolicy;
		savingPreferences = true;
		try {
			await api.profile.server.setSyncDialogPreferences({
				scope: nextScope,
				restartPolicy: nextRestartPolicy
			});
		} catch (error) {
			scope = previous.scope;
			restartPolicy = previous.restartPolicy;
			await message(error instanceof Error ? error.message : String(error), {
				title: m.serverSync_title(),
				kind: 'error'
			});
		} finally {
			savingPreferences = false;
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
				if (decision === 'restore') restoreConfigs.push(path);
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

	async function previewSync() {
		previewing = true;
		result = null;
		try {
			const selected = selection();
			const policy = restartPolicy;
			// The restart policy is bound into the plan hash, so the approval
			// is only valid while this selection stands.
			preview = await api.profile.server.previewSync(selected, policy, remotePassword, workerToken);
			policyOverrides = {};
			approvedInput = JSON.stringify({ selection: selected, restartPolicy: policy });
		} finally {
			previewing = false;
			stageProgress = null;
		}
	}

	function decide(path: string, decision: Decision | null) {
		if (decision === null) delete decisions[path];
		else decisions[path] = decision;
	}

	/// A persistent per-file policy for *future* revisions, distinct from
	/// the one-time Apply/Decline decision for the current conflict.
	async function setPolicy(path: string, policy: SyncConfigUpdatePolicy) {
		approvedInput = '';
		savingPolicy = true;
		policyOverrides[path] = policy;
		try {
			await api.profile.server.setConfigPolicy(path, policy, remotePassword, workerToken);
			// The command persists the policy but returns nothing, so the
			// confirmed value is folded into the rendered preview here.
			const entry = preview?.plan.configEntries.find((entry) => entry.path === path);
			if (entry) entry.policy = policy;
		} catch (error) {
			// The write never landed — drop the pending pick so the control
			// falls back to the last confirmed policy instead of implying an
			// unsaved value.
			delete policyOverrides[path];
			await message(error instanceof Error ? error.message : String(error), {
				title: m.serverSync_title(),
				kind: 'error'
			});
		} finally {
			delete policyOverrides[path];
			savingPolicy = false;
		}
	}

	async function acknowledgeRestart() {
		if (!(await confirm(m.serverSync_acknowledgeRestartConfirm()))) return;
		acknowledgingRestart = true;
		try {
			await api.profile.server.acknowledgeExternalRestart(remotePassword, workerToken);
			preview = null;
			approvedInput = '';
			await loadStatus(true);
		} catch (error) {
			await message(error instanceof Error ? error.message : String(error), {
				title: m.serverSync_acknowledgeRestartFailed(),
				kind: 'error'
			});
		} finally {
			acknowledgingRestart = false;
		}
	}

	async function deploy(force = false) {
		if (!preview || dirty) return;
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
			// The worker answers with the state it confirmed — reflect that,
			// not just the submitted values.
			const confirmed = await api.profile.server.configureWorker(
				workerAutoSync,
				workerAutoMods,
				workerRestartPolicy,
				workerToken
			);
			workerAutoSync = confirmed.autoSync;
			workerAutoMods = confirmed.autoMods;
			workerRestartPolicy = confirmed.restartPolicy;
			await loadStatus(false);
		} catch {
			// The push failed — snap the controls back to the worker's
			// last-known state instead of leaving intent it never ran.
			if (status?.worker) {
				workerAutoSync = status.worker.autoSync;
				workerAutoMods = status.worker.autoMods;
				workerRestartPolicy = status.worker.restartPolicy;
			}
		} finally {
			savingWorker = false;
		}
	}

	/// What of the pending publication still needs to reach the server.
	/// `pendingRevision` alone no longer implies a full deploy — a
	/// config-only pass may already have run, leaving only mods owed.
	function pendingScopeLabel(worker: WorkerStatus): string {
		const revision = new Date(worker.pendingRevision!).toLocaleString();
		// Workers built before phase-scoped status emit neither flag —
		// show the generic pending line rather than guess a scope.
		if (worker.pendingMods === undefined && worker.pendingConfigs === undefined) {
			return m.serverSync_pendingRevision({ revision });
		}
		if (worker.pendingMods && worker.pendingConfigs) {
			return m.serverSync_pendingRevision({ revision });
		}
		if (worker.pendingMods) {
			return m.serverSync_pendingMods({ revision });
		}
		return m.serverSync_pendingConfigs({ revision });
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

<Dialog title={m.serverSync_title()} bind:open canClose={!busy} large>
	<p class="text-primary-600 dark:text-primary-300 mt-1">{m.serverSync_content()}</p>

	{#if status}
		<div
			class="border-primary-300 dark:border-primary-600 bg-primary-50 dark:bg-primary-900 mt-4 rounded-lg border p-3 text-sm"
		>
			<div class="flex items-center justify-between gap-3">
				<span class="text-primary-700 dark:text-primary-300 font-medium">
					{isWorker() ? m.serverSync_modeWorker() : m.serverSync_modeLocal()}
				</span>
				<Button loading={loadingStatus} disabled={busy} onclick={() => loadStatus(true)}>
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
						<div class="flex flex-wrap items-center justify-between gap-2">
							<span class="text-orange-600 dark:text-orange-400"
								>{m.serverSync_restartRequired()}</span
							>
							<Button disabled={busy} loading={acknowledgingRestart} onclick={acknowledgeRestart}>
								{m.serverSync_acknowledgeRestart()}
							</Button>
						</div>
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
						{pendingScopeLabel(status.worker)}
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
						<Label for={`${formId}-field-1`}>{m.serverSync_autoSync()}</Label>
						<Checkbox id={`${formId}-field-1`} bind:checked={workerAutoSync} disabled={busy} />
					</div>
					<div class="flex items-center">
						<Label for={`${formId}-field-2`}>{m.serverSync_autoMods()}</Label>
						<Checkbox id={`${formId}-field-2`} bind:checked={workerAutoMods} disabled={busy} />
					</div>
					<Button color="primary" loading={savingWorker} disabled={busy} onclick={saveWorkerConfig}>
						{m.serverSync_saveAutomation()}
					</Button>
				</div>
			</details>
		{/if}
	{/if}

	<div class="mt-4">
		<Label for={`${formId}-field-3`}>{m.serverSync_scope()}</Label>
		<Select
			id={`${formId}-field-3`}
			type="single"
			triggerClass="mt-1 w-full"
			value={scope}
			onValueChange={(value) => savePreferences(value as DeployScope, restartPolicy)}
			disabled={busy || loadingPreferences}
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
					<Label for={`${formId}-field-4`}>{m.serverSync_workerToken()}</Label>
					<InputField
						id={`${formId}-field-4`}
						class="mt-1 w-full"
						bind:value={workerToken}
						type="password"
						disabled={busy}
					/>
					<p class="text-primary-500 mt-1 text-sm">{m.serverSync_savedCredential()}</p>
				</div>
			{:else}
				<div>
					<Label for={`${formId}-field-5`}>{m.dedicatedServerDialog_password()}</Label>
					<InputField
						id={`${formId}-field-5`}
						class="mt-1 w-full"
						bind:value={remotePassword}
						type="password"
						disabled={busy}
					/>
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
					<div class="mt-2 flex flex-wrap items-center justify-between gap-2 text-sm">
						<span class="text-orange-600 dark:text-orange-400">
							{m.serverSync_reviewRemaining({ count: remainingDecisions })}
						</span>
						<Button disabled={busy} onclick={() => (reviewOnly = !reviewOnly)}>
							{reviewOnly ? m.serverSync_showAllConfigs() : m.serverSync_reviewOnly()}
						</Button>
					</div>
					<p class="text-primary-600 dark:text-primary-300 mt-2 text-sm">
						{m.serverSync_policyHelp()}
					</p>
					<div
						class="border-primary-300 dark:border-primary-600 bg-primary-50 dark:bg-primary-900 mt-2 max-h-64 overflow-auto rounded-lg border p-3 text-sm"
					>
						{#each visibleConfigEntries as entry (entry.path)}
							<div
								class="flex flex-wrap items-center gap-2 py-2"
								data-testid="server-config-row"
								data-path={entry.path}
							>
								<span
									class="text-primary-700 dark:text-primary-300 min-w-0 grow basis-full font-mono wrap-anywhere xl:basis-40"
								>
									{entry.path}
								</span>
								<span class="text-primary-500 shrink-0">{actionLabel(entry)}</span>
								<div class="flex shrink-0 flex-col gap-1">
									<span class="text-primary-600 dark:text-primary-300 text-xs" aria-hidden="true"
										>{m.serverSync_futureUpdates()}</span
									>
									<Select
										type="single"
										disabled={busy}
										triggerClass="w-52 shrink-0"
										aria-label={m.serverSync_futurePolicy({ path: entry.path })}
										value={policyOverrides[entry.path] ?? entry.policy}
										onValueChange={(value) =>
											setPolicy(entry.path, value as SyncConfigUpdatePolicy)}
										items={[
											{ value: 'ask', label: m.serverSync_policyAsk() },
											{ value: 'alwaysApply', label: m.serverSync_policyAlwaysApply() },
											{ value: 'alwaysKeep', label: m.serverSync_policyAlwaysKeep() }
										]}
									/>
								</div>
								<div class="ml-auto flex shrink-0 items-center gap-2">
									{#if decisions[entry.path]}
										<span class="shrink-0 text-green-600 dark:text-green-400">
											{decisions[entry.path] !== 'decline'
												? m.serverSync_willApply()
												: m.serverSync_willDecline()}
										</span>
										<Button
											disabled={busy}
											aria-label={`${m.serverSync_undo()} ${entry.path}`}
											onclick={() => decide(entry.path, null)}
										>
											{m.serverSync_undo()}
										</Button>
									{:else if entry.action !== 'markApplied' && entry.action !== 'write'}
										{@const restore =
											entry.action === 'pending' && entry.reason === 'deletedLocally'}
										<Button
											disabled={busy}
											aria-label={`${restore ? m.serverSync_restore() : m.serverSync_apply()} ${entry.path}`}
											onclick={() => decide(entry.path, restore ? 'restore' : 'apply')}
										>
											{restore ? m.serverSync_restore() : m.serverSync_apply()}
										</Button>
										<Button
											disabled={busy}
											aria-label={`${m.serverSync_decline()} ${entry.path}`}
											onclick={() => decide(entry.path, 'decline')}
										>
											{m.serverSync_decline()}
										</Button>
									{/if}
								</div>
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
			{:else if result.plan.modsPhase && result.plan.configsPhase}
				<InfoBox type="info">{m.serverSync_deployed_done()}</InfoBox>
			{:else if result.plan.modsPhase}
				<InfoBox type="info">{m.serverSync_deployed_modsDone()}</InfoBox>
			{:else}
				<InfoBox type="info">{m.serverSync_deployed_configsDone()}</InfoBox>
			{/if}
			<DeploymentStats
				uploaded={result.summary.uploadedFiles}
				bytes={result.summary.uploadedBytes}
				removed={result.summary.removedFiles}
				unchanged={result.summary.unchangedFiles}
			/>
			{#if result.restart !== 'notRequired'}
				<p class="text-primary-600 dark:text-primary-300 mt-2 text-sm">
					{restartLabel(result.restart)}
				</p>
			{/if}
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

	<p class="text-primary-600 dark:text-primary-300 mt-4 min-h-5 text-sm" role="status">
		{#if preview}
			{dirty ? m.serverSync_dirtyHint() : m.serverSync_readyHint()}
		{:else if !result}
			{m.serverSync_previewHint()}
		{/if}
	</p>
	<div class="mt-3 flex w-full flex-wrap items-center justify-end gap-3">
		<Button class="mr-auto" color="primary" disabled={busy} onclick={() => (open = false)}
			>{m.serverSync_close()}</Button
		>
		<div class="order-first flex w-full flex-wrap items-center gap-2">
			<Label class="w-auto min-w-0" for={`${formId}-field-6`}>{m.serverSync_restartPolicy()}</Label>
			<Select
				id={`${formId}-field-6`}
				type="single"
				triggerClass="w-40"
				value={restartPolicy}
				onValueChange={(value) => savePreferences(scope, value as RestartPolicy)}
				disabled={busy || loadingPreferences}
				items={[
					{ value: 'manual', label: m.serverSync_restartManual() },
					{ value: 'immediate', label: m.serverSync_restartImmediate() },
					{ value: 'whenEmpty', label: m.serverSync_restartWhenEmpty() }
				]}
			/>
		</div>
		<Button icon="mdi:cloud-search" loading={previewing} disabled={busy} onclick={previewSync}>
			{m.serverSync_preview()}
		</Button>
		{#if preview?.busy?.stale}
			<Button
				icon="mdi:cloud-upload"
				loading={deploying}
				disabled={dirty || busy}
				onclick={() => deploy(true)}
			>
				{m.serverSync_takeover()}
			</Button>
		{:else}
			<Button
				icon="mdi:cloud-upload"
				loading={deploying}
				disabled={!preview || dirty || !!preview.busy || busy}
				onclick={() => deploy()}
			>
				{m.serverSync_deploy()}
			</Button>
		{/if}
	</div>
</Dialog>
