<script lang="ts">
	import Button from '$lib/components/ui/Button.svelte';
	import Select from '$lib/components/ui/Select.svelte';
	import Label from '$lib/components/ui/Label.svelte';
	import InfoBox from '$lib/components/ui/InfoBox.svelte';
	import DeploymentStats from './DeploymentStats.svelte';
	import ServerSyncOperationProgress from './ServerSyncOperationProgress.svelte';
	import type { RestartPolicy } from '$lib/types';
	import type { SyncConfigUpdatePolicy } from '$lib/types';
	import { m } from '$lib/paraglide/messages';
	import type { ServerFormState } from './serverForm.svelte';
	import type { RemoteSync } from './remoteSync.svelte';

	let { form, sync }: { form: ServerFormState; sync: RemoteSync } = $props();
	const formId = $props.id();

	/// Deploy needs the stored settings to match the form. A typed
	/// credential does not block it — the sync requests pick it up so the
	/// user can preview before saving.
	const settingsDirty = $derived(!form.hasSavedSettings || form.settingsChanged);
	/// Nothing published means there is nothing to preview or deploy.
	const noPublication = $derived(sync.status != null && sync.status.publicationRevision == null);
</script>

{#if noPublication}
	<InfoBox type="info" class="mt-3">{m.serverSync_notPublished()}</InfoBox>
{/if}

{#if sync.activeRun || sync.failedOperation}
	<ServerSyncOperationProgress
		operation={sync.activeRun?.operation ??
			sync.failedOperation ??
			sync.progress?.operation ??
			'preview'}
		progress={sync.progress}
		failed={sync.failedOperation !== null}
		elapsedSeconds={sync.elapsedSeconds}
	/>
{/if}

{#if sync.preview}
	<div class="mt-4">
		<DeploymentStats
			uploaded={sync.preview.plan.uploads.length}
			bytes={sync.preview.plan.uploadBytes}
			removed={sync.preview.plan.removals.length}
			unchanged={sync.preview.plan.unchangedFiles}
		/>

		{#if sync.noChanges}
			<InfoBox type="info" class="mt-3">{m.serverSync_noChanges()}</InfoBox>
		{/if}
		{#if sync.preview.busy}
			<InfoBox type="warning" class="mt-3">
				{m.serverSync_leaseHeld({ owner: sync.preview.busy.record.owner })}
			</InfoBox>
		{/if}
		{#each sync.preview.warnings as warning}
			<InfoBox type="warning" class="mt-3">{warning}</InfoBox>
		{/each}
		{#if sync.preview.plan.unmanaged.length > 0}
			<InfoBox type="info" class="mt-3">
				{m.serverSync_unmanaged({ count: sync.preview.plan.unmanaged.length })}
				<details class="mt-1">
					{#each sync.preview.plan.unmanaged as path}
						<div class="font-mono wrap-anywhere">{path}</div>
					{/each}
				</details>
			</InfoBox>
		{/if}
		{#if sync.preview.plan.requiresRestart}
			<InfoBox type="info" class="mt-3">{m.serverSync_willRestart()}</InfoBox>
		{/if}

		{#if sync.preview.plan.configEntries.length > 0}
			<details class="mt-3" open={sync.preview.plan.conflicts.length > 0}>
				<summary class="text-primary-700 dark:text-primary-300 cursor-pointer font-medium">
					{m.serverSync_configFiles({ count: sync.preview.plan.configEntries.length })}
				</summary>
				<div class="mt-2 flex flex-wrap items-center justify-between gap-2 text-sm">
					<span class="text-orange-600 dark:text-orange-400">
						{m.serverSync_reviewRemaining({ count: sync.remainingDecisions })}
					</span>
					<Button disabled={sync.busy} onclick={() => (sync.reviewOnly = !sync.reviewOnly)}>
						{sync.reviewOnly ? m.serverSync_showAllConfigs() : m.serverSync_reviewOnly()}
					</Button>
				</div>
				<p class="text-primary-600 dark:text-primary-300 mt-2 text-sm">
					{m.serverSync_policyHelp()}
				</p>
				<div
					class="border-primary-300 dark:border-primary-600 bg-primary-50 dark:bg-primary-900 mt-2 max-h-64 overflow-auto rounded-lg border p-3 text-sm"
				>
					{#each sync.visibleConfigEntries as entry (entry.path)}
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
							<span class="text-primary-500 shrink-0">{sync.actionLabel(entry)}</span>
							<div class="flex shrink-0 flex-col gap-1">
								<span class="text-primary-600 dark:text-primary-300 text-xs" aria-hidden="true"
									>{m.serverSync_futureUpdates()}</span
								>
								<Select
									type="single"
									disabled={sync.busy}
									triggerClass="w-52 shrink-0"
									aria-label={m.serverSync_futurePolicy({ path: entry.path })}
									value={sync.policyOverrides[entry.path] ?? entry.policy}
									onValueChange={(value) =>
										sync.setPolicy(entry.path, value as SyncConfigUpdatePolicy)}
									items={[
										{ value: 'ask', label: m.serverSync_policyAsk() },
										{ value: 'alwaysApply', label: m.serverSync_policyAlwaysApply() },
										{ value: 'alwaysKeep', label: m.serverSync_policyAlwaysKeep() }
									]}
								/>
							</div>
							<div class="ml-auto flex shrink-0 items-center gap-2">
								{#if sync.decisions[entry.path]}
									<span class="shrink-0 text-green-600 dark:text-green-400">
										{sync.decisions[entry.path] !== 'decline'
											? m.serverSync_willApply()
											: m.serverSync_willDecline()}
									</span>
								{/if}
								{#if entry.action !== 'markApplied' && entry.action !== 'write'}
									{@const restore = entry.action === 'pending' && entry.reason === 'deletedLocally'}
									<Button
										disabled={sync.busy}
										style={sync.decisions[entry.path] === 'decline' ? 'display: none' : undefined}
										aria-label={`${sync.decisions[entry.path] ? m.serverSync_undo() : restore ? m.serverSync_restore() : m.serverSync_apply()} ${entry.path}`}
										onclick={() =>
											sync.decide(
												entry.path,
												sync.decisions[entry.path] ? null : restore ? 'restore' : 'apply'
											)}
									>
										{sync.decisions[entry.path]
											? m.serverSync_undo()
											: restore
												? m.serverSync_restore()
												: m.serverSync_apply()}
									</Button>
									<Button
										disabled={sync.busy}
										style={sync.decisions[entry.path] && sync.decisions[entry.path] !== 'decline'
											? 'display: none'
											: undefined}
										aria-label={`${sync.decisions[entry.path] === 'decline' ? m.serverSync_undo() : m.serverSync_decline()} ${entry.path}`}
										onclick={() =>
											sync.decide(
												entry.path,
												sync.decisions[entry.path] === 'decline' ? null : 'decline'
											)}
									>
										{sync.decisions[entry.path] === 'decline'
											? m.serverSync_undo()
											: m.serverSync_decline()}
									</Button>
								{/if}
							</div>
						</div>
					{/each}
				</div>
			</details>
		{/if}

		{#if sync.preview.plan.uploads.length > 0 || sync.preview.plan.removals.length > 0}
			<details class="mt-3">
				<summary class="text-primary-700 dark:text-primary-300 cursor-pointer font-medium">
					{m.deploymentPreview_fileChanges({
						count: sync.preview.plan.uploads.length + sync.preview.plan.removals.length
					})}
				</summary>
				<div
					class="border-primary-300 dark:border-primary-600 bg-primary-50 dark:bg-primary-900 mt-2 max-h-64 overflow-auto rounded-lg border p-3 font-mono text-sm"
				>
					{#if sync.preview.plan.uploads.length > 0}
						<div class="text-primary-500 mb-1 font-sans font-semibold">
							{m.deploymentPreview_uploads()}
						</div>
						{#each sync.preview.plan.uploads as upload}
							<div class="text-primary-700 dark:text-primary-300 flex gap-2 py-0.5">
								<span class="shrink-0 text-green-600 dark:text-green-400">+</span>
								<span class="wrap-anywhere">{upload.path}</span>
							</div>
						{/each}
					{/if}
					{#if sync.preview.plan.removals.length > 0}
						<div class="text-primary-500 mt-3 mb-1 font-sans font-semibold">
							{m.deploymentPreview_removals()}
						</div>
						{#each sync.preview.plan.removals as path}
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

{#if sync.result}
	<div class="mt-4">
		{#if sync.result.state.lastOperation?.status === 'partial' || sync.result.failedConfigWrites.length > 0}
			<InfoBox type="warning">{m.serverSync_partialDone()}</InfoBox>
		{:else if sync.result.plan.modsPhase && sync.result.plan.configsPhase}
			<InfoBox type="info">{m.serverSync_deployed_done()}</InfoBox>
		{:else if sync.result.plan.modsPhase}
			<InfoBox type="info">{m.serverSync_deployed_modsDone()}</InfoBox>
		{:else}
			<InfoBox type="info">{m.serverSync_deployed_configsDone()}</InfoBox>
		{/if}
		<DeploymentStats
			uploaded={sync.result.summary.uploadedFiles}
			bytes={sync.result.summary.uploadedBytes}
			removed={sync.result.summary.removedFiles}
			unchanged={sync.result.summary.unchangedFiles}
		/>
		{#if sync.result.restart !== 'notRequired'}
			<p class="text-primary-600 dark:text-primary-300 mt-2 text-sm">
				{sync.restartLabel(sync.result.restart)}
			</p>
		{/if}
		{#if sync.result.failedConfigWrites.length > 0}
			<InfoBox type="warning" class="mt-2">
				{m.serverSync_failedConfigs({ count: sync.result.failedConfigWrites.length })}
			</InfoBox>
		{/if}
		{#each sync.result.warnings as warning}
			<InfoBox type="warning" class="mt-2">{warning}</InfoBox>
		{/each}
	</div>
{/if}

<p class="text-primary-600 dark:text-primary-300 mt-4 min-h-5 text-sm" role="status">
	{#if settingsDirty}
		{form.hasSavedSettings ? m.serverPage_saveToDeployDirty() : m.serverPage_saveToDeploy()}
	{:else if sync.preview}
		{#if sync.previewDirty}
			{m.serverSync_dirtyHint()}
		{:else if sync.noChanges}
			{m.serverSync_noChangesHint()}
		{:else}
			{m.serverSync_readyHint()}
		{/if}
	{:else if !sync.result}
		{m.serverSync_previewHint()}
	{/if}
</p>
<div class="mt-3 flex w-full flex-wrap items-center gap-3">
	<div class="flex items-center gap-2">
		<Label class="w-auto min-w-0" for={`${formId}-restart`}>{m.serverSync_restartPolicy()}</Label>
		<Select
			id={`${formId}-restart`}
			type="single"
			triggerClass="w-40"
			value={sync.restartPolicy}
			onValueChange={(value) => sync.savePreferences(value as RestartPolicy)}
			disabled={sync.busy || sync.loadingPreferences || settingsDirty}
			items={[
				{ value: 'manual', label: m.serverSync_restartManual() },
				{ value: 'immediate', label: m.serverSync_restartImmediate() },
				{ value: 'whenEmpty', label: m.serverSync_restartWhenEmpty() }
			]}
		/>
	</div>
	<div class="ml-auto flex flex-wrap items-center gap-2">
		<Button
			icon="mdi:cloud-search"
			loading={sync.previewing && sync.previewScope === 'mods'}
			disabled={sync.busy || settingsDirty || noPublication}
			onclick={() => sync.previewSync()}
		>
			{m.serverSync_preview()}
		</Button>
		{#if sync.preview?.busy?.stale}
			<Button
				icon="mdi:cloud-upload"
				loading={sync.deploying}
				disabled={sync.previewDirty || sync.busy || settingsDirty || noPublication}
				onclick={() => sync.deploy(true)}
			>
				{m.serverSync_takeover()}
			</Button>
		{:else}
			<Button
				icon="mdi:cloud-upload"
				loading={sync.deploying}
				disabled={!sync.preview ||
					sync.previewDirty ||
					!!sync.preview.busy ||
					sync.busy ||
					settingsDirty ||
					noPublication}
				onclick={() => sync.deploy()}
			>
				{sync.previewScope === 'configs' ? m.serverSync_deployConfigs() : m.serverSync_deploy()}
			</Button>
		{/if}
	</div>
</div>

<details class="mt-4">
	<summary class="text-primary-600 dark:text-primary-300 cursor-pointer">
		{m.serverSync_configSection()}
	</summary>
	<div class="mt-2 flex flex-col gap-3">
		<p class="text-primary-600 dark:text-primary-300 text-sm">
			{m.serverSync_configSectionHelp()}
		</p>
		<Button
			icon="mdi:cloud-search"
			loading={sync.previewing && sync.previewScope === 'configs'}
			disabled={sync.busy || settingsDirty || noPublication}
			onclick={() => sync.previewSync(true)}
		>
			{m.serverSync_previewConfigs()}
		</Button>
	</div>
</details>
