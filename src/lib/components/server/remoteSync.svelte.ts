import * as api from '$lib/api';
import type {
	DeploySelection,
	PlanConfigEntry,
	RestartPolicy,
	ServerSyncPreview,
	ServerSyncOperationProgress as OperationProgress,
	ServerSyncResult,
	ServerSyncStatus,
	SyncConfigUpdatePolicy
} from '$lib/types';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { confirm, message } from '@tauri-apps/plugin-dialog';
import serverSync from '$lib/state/serverSync.svelte';
import { m } from '$lib/paraglide/messages';
import type { ServerFormState } from './serverForm.svelte';

/// What the user decided for one config path.
type Decision = 'apply' | 'restore' | 'decline';

/// The status/preview/deploy state machine for the remote tab, formerly
/// the ServerSyncDialog's script. Credentials come from the connection
/// fields on the same page — typed-but-unsaved values are passed so the
/// user can preview before saving.
export class RemoteSync {
	#form: ServerFormState;

	/// Which payload the current preview targets. Mods are the routine
	/// deployment; configs are a separate explicit push — the server owns
	/// its config files after setup, so they are never bundled into the
	/// primary sync operation.
	previewScope = $state<'mods' | 'configs'>('mods');
	decisions = $state<Record<string, Decision>>({});
	status = $state<ServerSyncStatus | null>(null);
	preview = $state<ServerSyncPreview | null>(null);
	result = $state<ServerSyncResult | null>(null);
	restartPolicy = $state<RestartPolicy>('manual');
	approvedInput = $state('');
	currentInput = $derived(
		JSON.stringify({ selection: this.selection(), restartPolicy: this.restartPolicy })
	);
	previewDirty = $derived(!this.preview || this.approvedInput !== this.currentInput);
	loadingStatus = $state(false);
	previewing = $state(false);
	deploying = $state(false);
	progress = $state<OperationProgress | null>(null);
	activeRun = $state<{ id: string; operation: 'preview' | 'deploy' } | null>(null);
	failedOperation = $state<'preview' | 'deploy' | null>(null);
	elapsedSeconds = $state(0);
	lastRefreshAt = $state<Date | null>(null);
	/// Set once the first live refresh attempt settles, success or
	/// failure — "checking" is only honest while that call is in flight.
	liveChecked = $state(false);
	#startedAt = 0;
	#elapsedTimer: ReturnType<typeof setInterval> | null = null;
	#workerTimer: ReturnType<typeof setInterval> | null = null;
	#workerPollingRunId: string | null = null;
	/// Policy picks still being confirmed by the backend. While a save is in
	/// flight the override is what the dropdown shows; once it lands the value
	/// is folded into the preview entry, and on failure the override is dropped
	/// so the control falls back to the last confirmed policy.
	policyOverrides = $state<Record<string, SyncConfigUpdatePolicy>>({});
	acknowledgingRestart = $state(false);
	reviewOnly = $state(false);
	loadingPreferences = $state(false);
	savingPreferences = $state(false);
	savingPolicy = $state(false);
	mounted = $state(false);
	#unlisten: UnlistenFn | null = null;

	busy = $derived(
		this.previewing ||
			this.deploying ||
			this.savingPolicy ||
			this.savingPreferences ||
			this.acknowledgingRestart
	);
	remainingDecisions = $derived(
		this.preview?.plan.configEntries.filter(
			(entry) => entry.action === 'pending' && !this.decisions[entry.path]
		).length ?? 0
	);
	noChanges = $derived(
		this.previewScope === 'mods' &&
			this.preview != null &&
			this.preview.plan.uploads.length === 0 &&
			this.preview.plan.removals.length === 0
	);
	visibleConfigEntries = $derived.by(() => {
		const entries = this.preview?.plan.configEntries ?? [];
		return entries
			.filter((entry) => !this.reviewOnly || entry.action === 'pending')
			.sort((a, b) => Number(b.action === 'pending') - Number(a.action === 'pending'));
	});

	constructor(form: ServerFormState) {
		this.#form = form;
	}

	get password() {
		return this.#form.remotePassword;
	}

	get workerToken() {
		return this.#form.workerToken;
	}

	/// Called when the remote tab becomes visible. Returns the teardown.
	mount() {
		this.mounted = true;
		void this.loadPreferences();
		void this.loadStatus(true).catch(() => {});
		void listen<OperationProgress>('server_sync_operation_progress', (event) =>
			this.acceptProgress(event.payload)
		).then((unlisten) => (this.#unlisten = unlisten));

		return () => {
			this.mounted = false;
			this.#unlisten?.();
			this.#unlisten = null;
			this.stopProgressTimers();
			this.activeRun = null;
			this.failedOperation = null;
			this.decisions = {};
			this.reviewOnly = false;
			this.preview = null;
			this.policyOverrides = {};
			this.result = null;
			this.progress = null;
			this.approvedInput = '';
			this.liveChecked = false;
		};
	}

	stopProgressTimers() {
		if (this.#elapsedTimer) clearInterval(this.#elapsedTimer);
		if (this.#workerTimer) clearInterval(this.#workerTimer);
		this.#elapsedTimer = null;
		this.#workerTimer = null;
	}

	acceptProgress(update: OperationProgress) {
		if (
			!this.mounted ||
			this.activeRun?.id !== update.runId ||
			this.activeRun.operation !== update.operation
		)
			return;
		if (this.progress?.status === 'failed' && update.status === 'running') return;
		if (this.progress && update.completedPhases < this.progress.completedPhases) return;
		if (
			this.progress &&
			update.completedPhases === this.progress.completedPhases &&
			update.completed < this.progress.completed
		)
			return;
		this.progress = update;
	}

	async pollWorkerProgress(runId: string) {
		if (this.#workerPollingRunId === runId || this.activeRun?.id !== runId) return;
		this.#workerPollingRunId = runId;
		try {
			const update = await api.profile.server.getSyncProgress(this.workerToken);
			if (update) this.acceptProgress(update);
		} catch {
			// Status delivery is observational. The operation request owns errors.
		} finally {
			if (this.#workerPollingRunId === runId) this.#workerPollingRunId = null;
		}
	}

	beginOperation(operation: 'preview' | 'deploy'): string {
		this.stopProgressTimers();
		const id = crypto.randomUUID();
		this.activeRun = { id, operation };
		this.failedOperation = null;
		this.progress = null;
		this.#startedAt = Date.now();
		this.elapsedSeconds = 0;
		this.#elapsedTimer = setInterval(() => {
			if (this.activeRun?.id === id)
				this.elapsedSeconds = Math.floor((Date.now() - this.#startedAt) / 1000);
		}, 1000);
		if (this.isWorker()) {
			this.#workerTimer = setInterval(() => void this.pollWorkerProgress(id), 500);
		}
		return id;
	}

	async failOperation(id: string, operation: 'preview' | 'deploy') {
		if (this.activeRun?.id !== id || !this.mounted) return;
		if (this.isWorker()) {
			try {
				const final = await api.profile.server.getSyncProgress(this.workerToken);
				if (final) this.acceptProgress(final);
			} catch {
				// Keep the last delivered phase if the status request fails.
			}
		}
		if (this.activeRun?.id !== id || !this.mounted) return;
		if (this.progress) this.progress = { ...this.progress, status: 'failed' };
		this.failedOperation = operation;
		this.activeRun = null;
		this.stopProgressTimers();
	}

	completeOperation(id: string) {
		if (this.activeRun?.id !== id || !this.mounted) return false;
		this.activeRun = null;
		this.failedOperation = null;
		this.progress = null;
		this.stopProgressTimers();
		return true;
	}

	async loadStatus(refresh: boolean) {
		this.loadingStatus = true;
		try {
			const next = await api.profile.server.getSyncStatus(refresh, this.password, this.workerToken);
			// A non-refresh response carries only publication metadata in
			// either mode — keep the last live server state.
			if (!refresh && next.server == null) {
				next.server = this.status?.server ?? null;
			}
			if (!refresh && next.worker && next.worker.server == null) {
				next.worker.server = this.status?.worker?.server ?? null;
			}
			this.status = next;
			serverSync.record(next);
			this.lastRefreshAt = new Date();
		} finally {
			this.loadingStatus = false;
			if (refresh) this.liveChecked = true;
		}
	}

	/// Drops everything tied to the previous profile — status, preview,
	/// results — so a profile switch never leaks the old profile's state.
	/// The panel's own mount() is not re-run, so preferences reload here.
	reset() {
		this.stopProgressTimers();
		this.status = null;
		this.preview = null;
		this.result = null;
		this.decisions = {};
		this.policyOverrides = {};
		this.progress = null;
		this.activeRun = null;
		this.failedOperation = null;
		this.approvedInput = '';
		this.reviewOnly = false;
		this.restartPolicy = 'manual';
		this.lastRefreshAt = null;
		this.liveChecked = false;
		serverSync.record(null);
		void this.loadPreferences();
	}

	async loadPreferences() {
		this.loadingPreferences = true;
		try {
			const preferences = await api.profile.server.getSyncDialogPreferences();
			this.restartPolicy = preferences.restartPolicy;
		} finally {
			this.loadingPreferences = false;
		}
	}

	async savePreferences(nextRestartPolicy: RestartPolicy) {
		const previous = this.restartPolicy;
		this.restartPolicy = nextRestartPolicy;
		this.savingPreferences = true;
		try {
			await api.profile.server.setSyncDialogPreferences({
				restartPolicy: nextRestartPolicy
			});
		} catch (error) {
			this.restartPolicy = previous;
			await message(error instanceof Error ? error.message : String(error), {
				title: m.serverPage_title(),
				kind: 'error'
			});
		} finally {
			this.savingPreferences = false;
		}
	}

	isWorker() {
		return this.status?.mode === 'worker';
	}

	selection(): DeploySelection {
		const applyConfigs: string[] = [];
		const restoreConfigs: string[] = [];
		const declineConfigs: string[] = [];

		// A mods operation must serialize as literally config-free. Only
		// the config workflow may carry decisions into the selection.
		if (this.previewScope === 'configs') {
			for (const [path, decision] of Object.entries(this.decisions)) {
				if (decision === 'decline') {
					declineConfigs.push(path);
				} else {
					applyConfigs.push(path);
					// Restoring recreates a file the server side deleted; the
					// planner requires the extra authorization for that case.
					if (decision === 'restore') restoreConfigs.push(path);
				}
			}
		}

		return {
			includeMods: this.previewScope === 'mods',
			includeConfigs: this.previewScope === 'configs',
			applyConfigs,
			restoreConfigs,
			declineConfigs
		};
	}

	async previewSync(configs = false) {
		this.previewScope = configs ? 'configs' : 'mods';
		if (!configs) this.decisions = {};
		const runId = this.beginOperation('preview');
		this.previewing = true;
		this.result = null;
		try {
			const selected = this.selection();
			const policy = this.restartPolicy;
			// The restart policy is bound into the plan hash, so the approval
			// is only valid while this selection stands.
			const operation = api.profile.server.previewSync(
				selected,
				policy,
				this.password,
				this.workerToken,
				runId
			);
			if (this.isWorker()) void this.pollWorkerProgress(runId);
			const nextPreview = await operation;
			if (!this.completeOperation(runId)) return;
			this.preview = nextPreview;
			this.policyOverrides = {};
			this.approvedInput = JSON.stringify({ selection: selected, restartPolicy: policy });
		} catch (error) {
			await this.failOperation(runId, 'preview');
			await message(error instanceof Error ? error.message : String(error), {
				title: m.serverSync_preview(),
				kind: 'error'
			});
		} finally {
			this.previewing = false;
		}
	}

	decide(path: string, decision: Decision | null) {
		if (decision === null) delete this.decisions[path];
		else this.decisions[path] = decision;
	}

	/// A persistent per-file policy for *future* revisions, distinct from
	/// the one-time Apply/Decline decision for the current conflict.
	async setPolicy(path: string, policy: SyncConfigUpdatePolicy) {
		this.approvedInput = '';
		this.savingPolicy = true;
		this.policyOverrides[path] = policy;
		try {
			await api.profile.server.setConfigPolicy(path, policy, this.password, this.workerToken);
			// The command persists the policy but returns nothing, so the
			// confirmed value is folded into the rendered preview here.
			const entry = this.preview?.plan.configEntries.find((entry) => entry.path === path);
			if (entry) entry.policy = policy;
		} catch (error) {
			// The write never landed — drop the pending pick so the control
			// falls back to the last confirmed policy instead of implying an
			// unsaved value.
			delete this.policyOverrides[path];
			await message(error instanceof Error ? error.message : String(error), {
				title: m.serverPage_title(),
				kind: 'error'
			});
		} finally {
			delete this.policyOverrides[path];
			this.savingPolicy = false;
		}
	}

	async acknowledgeRestart() {
		if (!(await confirm(m.serverSync_acknowledgeRestartConfirm()))) return;
		this.acknowledgingRestart = true;
		try {
			await api.profile.server.acknowledgeExternalRestart(this.password, this.workerToken);
			this.preview = null;
			this.approvedInput = '';
			await this.loadStatus(true);
		} catch (error) {
			await message(error instanceof Error ? error.message : String(error), {
				title: m.serverSync_acknowledgeRestartFailed(),
				kind: 'error'
			});
		} finally {
			this.acknowledgingRestart = false;
		}
	}

	async deploy(force = false) {
		if (!this.preview || this.previewDirty) return;
		const runId = this.beginOperation('deploy');
		this.deploying = true;
		try {
			const operation = api.profile.server.deploySync(
				this.selection(),
				this.preview.plan.hash,
				this.restartPolicy,
				force,
				this.password,
				this.workerToken,
				runId
			);
			if (this.isWorker()) void this.pollWorkerProgress(runId);
			const nextResult = await operation;
			if (!this.completeOperation(runId)) return;
			this.result = nextResult;
			this.decisions = {};
			this.preview = null;
			void this.loadStatus(true).catch(() => {});
		} catch (error) {
			await this.failOperation(runId, 'deploy');
			await message(error instanceof Error ? error.message : String(error), {
				title: m.serverSync_deployFailedTitle(),
				kind: 'error'
			});
		} finally {
			this.deploying = false;
		}
	}

	actionLabel(entry: PlanConfigEntry): string {
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

	restartLabel(outcome: string): string {
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
}
