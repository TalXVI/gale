<script lang="ts">
	import Button from '$lib/components/ui/Button.svelte';
	import InfoBox from '$lib/components/ui/InfoBox.svelte';
	import Spinner from '$lib/components/ui/Spinner.svelte';
	import profiles from '$lib/state/profile.svelte';
	import { remoteDeployState } from '$lib/state/serverSync.svelte';
	import { timeSince } from '$lib/util';
	import { onMount } from 'svelte';
	import { m } from '$lib/paraglide/messages';
	import type { ServerFormState } from './serverForm.svelte';
	import type { RemoteSync } from './remoteSync.svelte';

	let { form, sync }: { form: ServerFormState; sync: RemoteSync } = $props();

	onMount(() => sync.mount());

	const statusKind = $derived(remoteDeployState(sync.status, sync.liveChecked));
	const statusText = $derived(
		statusKind === 'pending' && sync.isWorker()
			? m.serverSync_statusPendingWorker()
			: {
					checking: m.serverSync_statusChecking(),
					unavailable: m.serverSync_statusUnavailable(),
					pending: m.serverSync_statusPending(),
					upToDate: m.serverSync_statusUpToDate(),
					neverDeployed: m.serverSync_statusNeverDeployed(),
					deployed: m.serverSync_statusDeployed()
				}[statusKind]
	);
	const statusClass = $derived(
		statusKind === 'pending'
			? 'text-orange-600 dark:text-orange-400'
			: statusKind === 'upToDate'
				? 'text-green-600 dark:text-green-400'
				: statusKind === 'unavailable'
					? 'text-red-600 dark:text-red-400'
					: 'text-primary-700 dark:text-primary-300'
	);

	/// Pending wording reflects what the worker will actually do — for the
	/// managed worker its own reported pending mode, otherwise the
	/// automation flags in the live worker status.
	const pendingText = $derived.by(() => {
		const worker = sync.status?.worker;
		if (statusKind !== 'pending' || !worker) return null;
		const hosted = form.localWorker?.pendingPublication;
		if (form.syncChoice === 'hostedWorker' && hosted) {
			return form.pendingLabel(hosted);
		}
		const mode = worker.autoSync ? (worker.autoMods ? 'automatic' : 'modsManual') : 'manual';
		return form.pendingLabel({
			mode,
			retrying: mode === 'automatic' && worker.nextAttemptAt != null
		});
	});

	const executorLabel = $derived.by(() => {
		if (!sync.isWorker()) return m.serverSync_viaDirect();
		if (form.syncChoice === 'hostedWorker') return m.serverSync_viaWorkerLocal();
		const address = form.form.remote.worker.address;
		try {
			return m.serverSync_viaWorker({ host: new URL(address).host || address });
		} catch {
			return m.serverSync_viaWorker({ host: address });
		}
	});

	const publishedAt = $derived(sync.status?.publicationRevision ?? null);
	const deployedAt = $derived(
		sync.status?.worker?.lastDeployedRevision ??
			sync.status?.server?.lastOperation?.finishedAt ??
			null
	);

	// Interval ticks never open a transport session — worker refresh=true
	// also hits the game host through the worker, so both modes only
	// re-read metadata while the page sits open.
	$effect(() => {
		const worker = sync.isWorker();
		const interval = setInterval(
			() => {
				if (document.visibilityState === 'visible' && !sync.busy) {
					void sync.loadStatus(false).catch(() => {});
				}
				if (worker && form.syncChoice === 'hostedWorker') void form.refreshLocalWorker();
			},
			worker ? 10_000 : 60_000
		);
		return () => clearInterval(interval);
	});

	// A new publication changes what the server should be running.
	let seenPublication = $state<string | null | undefined>(undefined);
	$effect(() => {
		const updatedAt = profiles.active?.sync?.updatedAt ?? null;
		if (seenPublication === undefined) {
			seenPublication = updatedAt;
			return;
		}
		if (updatedAt !== seenPublication) {
			seenPublication = updatedAt;
			if (!sync.busy) void sync.loadStatus(true).catch(() => {});
		}
	});

	function onVisibilityChange() {
		if (document.visibilityState !== 'visible' || sync.busy) return;
		// Refocus only re-reads metadata; a live check needs an explicit
		// Refresh or a new publication.
		void sync.loadStatus(false).catch(() => {});
	}
</script>

<svelte:document onvisibilitychange={onVisibilityChange} />

<div
	class="border-primary-300 dark:border-primary-600 bg-primary-50 dark:bg-primary-900 mt-4 rounded-lg border p-3 text-sm"
>
	<div class="flex items-center justify-between gap-3">
		<div class="flex min-w-0 items-center gap-2">
			{#if statusKind === 'checking'}
				<Spinner class="text-primary-500" />
			{/if}
			<span class={['font-medium', statusClass]}>
				{statusText}
			</span>
			<span class="text-primary-500 dark:text-primary-400 truncate">{executorLabel}</span>
		</div>
		<div class="flex shrink-0 items-center gap-2">
			{#if sync.lastRefreshAt}
				<span class="text-primary-500 text-xs" title={sync.lastRefreshAt.toLocaleString()}
					>{m.serverSync_updatedAgo({ time: timeSince(sync.lastRefreshAt) })}</span
				>
			{/if}
			<Button
				size="md"
				color="primary"
				loading={sync.loadingStatus}
				disabled={sync.busy}
				onclick={() => void sync.loadStatus(true).catch(() => {})}
			>
				{m.serverSync_refresh()}
			</Button>
		</div>
	</div>
	<div class="text-primary-600 dark:text-primary-400 mt-2 flex flex-col gap-1">
		{#if pendingText}
			<span class="text-orange-600 dark:text-orange-400">{pendingText}</span>
		{/if}
		<div class="flex flex-wrap gap-x-4">
			{#if publishedAt}
				<span title={new Date(publishedAt).toLocaleString()}>
					{m.serverSync_publishedAgo({ time: timeSince(publishedAt) })}
				</span>
			{/if}
			{#if deployedAt}
				<span title={new Date(deployedAt).toLocaleString()}>
					{m.serverSync_lastDeployedAgo({ time: timeSince(deployedAt) })}
				</span>
			{/if}
		</div>
	</div>
</div>

{#if sync.status?.server?.restartRequired}
	<InfoBox type="warning">
		<div class="flex flex-wrap items-center justify-between gap-2">
			<span>{m.serverSync_restartRequired()}</span>
			<Button
				disabled={sync.busy}
				loading={sync.acknowledgingRestart}
				onclick={() => sync.acknowledgeRestart()}
			>
				{m.serverSync_acknowledgeRestart()}
			</Button>
		</div>
	</InfoBox>
{/if}
{#if sync.status?.server?.lease}
	<InfoBox type="warning"
		>{m.serverSync_leaseHeld({ owner: sync.status.server.lease.owner })}</InfoBox
	>
{/if}
{#if sync.status?.worker?.busy}
	<InfoBox type="warning">{m.serverSync_workerBusy()}</InfoBox>
{/if}
{#if sync.status?.worker?.lastError}
	<InfoBox type="error">{sync.status.worker.lastError}</InfoBox>
{/if}
{#if sync.status?.worker?.pollError}
	<InfoBox type="error">{sync.status.worker.pollError}</InfoBox>
{/if}
{#each sync.status?.warnings ?? [] as warning (warning)}
	<InfoBox type="warning">{warning}</InfoBox>
{/each}
{#if sync.status?.credentialRequired}
	<InfoBox type="error">{m.serverSync_credentialRequired()}</InfoBox>
{/if}
