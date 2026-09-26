<script lang="ts">
	import type { ServerSyncOperationProgress, ServerSyncPhase } from '$lib/types';
	import { m } from '$lib/paraglide/messages';

	let {
		operation,
		progress,
		failed = false,
		elapsedSeconds
	}: {
		operation: 'preview' | 'deploy';
		progress: ServerSyncOperationProgress | null;
		failed?: boolean;
		elapsedSeconds: number;
	} = $props();

	const phaseNames: Record<ServerSyncPhase, () => string> = {
		fetchingPublication: m.serverSync_progressFetching,
		stagingPayload: m.serverSync_progressStaging,
		connecting: m.serverSync_progressConnecting,
		readingState: m.serverSync_progressReadingState,
		checkingLease: m.serverSync_progressCheckingLease,
		refreshingState: m.serverSync_progressRefreshingState,
		scanningPayload: m.serverSync_progressScanning,
		verifyingPayload: m.serverSync_progressVerifying,
		checkingConfigs: m.serverSync_progressCheckingConfigs,
		buildingPlan: m.serverSync_progressBuildingPlan,
		finalizingPreview: m.serverSync_progressFinalizing,
		removingFiles: m.serverSync_progressRemoving,
		uploadingPayload: m.serverSync_progressUploading,
		writingConfigs: m.serverSync_progressWritingConfigs,
		persistingState: m.serverSync_progressPersisting,
		applyingRestart: m.serverSync_progressRestarting,
		releasingLease: m.serverSync_progressReleasingLease
	};

	function unit(phase: ServerSyncPhase): string {
		if (phase === 'stagingPayload') return m.serverSync_progressMods();
		if (phase === 'scanningPayload' || phase === 'removingFiles')
			return m.serverSync_progressItems();
		return m.serverSync_progressFiles();
	}

	function bytes(value: number): string {
		if (value < 1000) return `${value} B`;
		if (value < 1000 * 1000) return `${(value / 1000).toFixed(1)} KB`;
		if (value < 1000 * 1000 * 1000) return `${(value / (1000 * 1000)).toFixed(1)} MB`;
		return `${(value / (1000 * 1000 * 1000)).toFixed(1)} GB`;
	}

	function elapsed(value: number): string {
		return `${Math.floor(value / 60)}:${String(value % 60).padStart(2, '0')}`;
	}
</script>

<section
	class="border-primary-300 dark:border-primary-600 bg-primary-50 dark:bg-primary-900 mt-4 min-w-0 rounded-lg border p-3"
	aria-label={m.serverSync_progressLabel()}
>
	<div class="flex min-w-0 items-start justify-between gap-3">
		<div class="min-w-0" role="status" aria-live="polite" aria-atomic="true">
			<p class="text-primary-900 dark:text-primary-100 font-semibold">
				{#if failed || progress?.status === 'failed'}
					{m.serverSync_progressFailedWhile({
						operation: operation === 'preview' ? m.serverSync_preview() : m.serverSync_deploy(),
						phase: progress
							? phaseNames[progress.phase]().toLowerCase()
							: m.serverSync_progressStarting().toLowerCase()
					})}
				{:else}
					{operation === 'preview'
						? m.serverSync_progressPreviewing()
						: m.serverSync_progressDeploying()}
				{/if}
			</p>
			<p class="text-primary-700 dark:text-primary-300 mt-0.5 text-sm">
				{progress ? phaseNames[progress.phase]() : m.serverSync_progressStarting()}
			</p>
		</div>
		<time
			class="text-primary-500 shrink-0 text-xs tabular-nums"
			aria-label={m.serverSync_progressElapsed()}
		>
			{elapsed(elapsedSeconds)}
		</time>
	</div>

	{#if progress}
		<div class="text-primary-600 dark:text-primary-400 mt-3 flex justify-between gap-2 text-xs">
			<span>{m.serverSync_progressOverall()}</span>
			<span class="tabular-nums">
				{m.serverSync_progressPhases({
					completed: progress.completedPhases,
					total: progress.totalPhases
				})}
			</span>
		</div>
		<div
			role="progressbar"
			aria-label={m.serverSync_progressOverall()}
			aria-valuemin="0"
			aria-valuemax={progress.totalPhases}
			aria-valuenow={progress.completedPhases}
			class="bg-primary-200 dark:bg-primary-700 mt-1 h-2 overflow-hidden rounded-full"
		>
			<div
				class="bg-primary-600 dark:bg-primary-300 h-full rounded-full transition-[width]"
				style:width={`${(progress.completedPhases / progress.totalPhases) * 100}%`}
			></div>
		</div>

		{#if progress.total !== null && progress.total > 0}
			<div
				class="text-primary-600 dark:text-primary-400 mt-3 flex flex-wrap justify-between gap-x-3 text-xs"
			>
				<span>{m.serverSync_progressCurrentPhase()}</span>
				<span class="tabular-nums">
					{progress.completed} / {progress.total}
					{unit(progress.phase)}
					{#if progress.totalBytes !== null && progress.completedBytes !== null}
						· {bytes(progress.completedBytes)} / {bytes(progress.totalBytes)}
					{/if}
				</span>
			</div>
			<div
				role="progressbar"
				aria-label={phaseNames[progress.phase]()}
				aria-valuemin="0"
				aria-valuemax={progress.total}
				aria-valuenow={progress.completed}
				class="bg-primary-200 dark:bg-primary-700 mt-1 h-2 overflow-hidden rounded-full"
			>
				<div
					class="bg-primary-600 dark:bg-primary-300 h-full rounded-full transition-[width]"
					style:width={`${(progress.completed / progress.total) * 100}%`}
				></div>
			</div>
		{:else if progress.total === null}
			<div
				role="progressbar"
				aria-label={phaseNames[progress.phase]()}
				class="bg-primary-200 dark:bg-primary-700 mt-3 h-2 overflow-hidden rounded-full"
			>
				<div
					class="bg-primary-600 dark:bg-primary-300 h-full w-1/3 rounded-full motion-reduce:animate-none"
					class:animate-pulse={!failed && progress.status === 'running'}
				></div>
			</div>
			{#if progress.completed > 0}
				<p class="text-primary-600 dark:text-primary-400 mt-2 text-xs tabular-nums">
					{progress.completed}
					{unit(progress.phase)}
					{m.serverSync_progressFound()}
				</p>
			{/if}
		{/if}
		{#if progress.item}
			<p
				class="text-primary-600 dark:text-primary-300 mt-2 min-w-0 truncate font-mono text-xs"
				title={progress.item}
			>
				{failed || progress.status === 'failed'
					? m.serverSync_progressLastItem()
					: ''}{progress.item}
			</p>
		{/if}
	{:else}
		<div
			role="progressbar"
			aria-label={m.serverSync_progressOverall()}
			class="bg-primary-200 dark:bg-primary-700 mt-3 h-2 overflow-hidden rounded-full"
		>
			<div
				class="bg-primary-600 dark:bg-primary-300 h-full w-1/3 rounded-full motion-reduce:animate-none"
				class:animate-pulse={!failed}
			></div>
		</div>
	{/if}
</section>
