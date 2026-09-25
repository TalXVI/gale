import * as api from '$lib/api';
import games from '$lib/state/game.svelte';
import profiles from '$lib/state/profile.svelte';
import type { ProfileServerSettings, ServerSyncStatus } from '$lib/types';

/// High-level remote deployment state, in the same terms the server
/// page's status line uses.
export type RemoteDeployState =
	/// Local mode before a live refresh has read the server.
	| 'checking'
	/// A publication newer than the deployed revision exists, or a worker
	/// reports pending work.
	| 'pending'
	| 'upToDate'
	| 'neverDeployed'
	/// A live refresh was attempted and produced no server state — the
	/// transport is unreachable, unconfigured, or the call failed.
	| 'unavailable'
	/// Mods are on the server but freshness is unknown — e.g. the last
	/// operation was a config push or predates publication tracking.
	| 'deployed';

export function remoteDeployState(
	status: ServerSyncStatus | null,
	liveChecked = false
): RemoteDeployState {
	if (!status) return liveChecked ? 'unavailable' : 'checking';
	// Nothing published means nothing is deployable — never claim
	// "up to date" without a publication to compare against.
	if (status.publicationRevision == null) return 'neverDeployed';
	if (status.worker) {
		if (status.worker.pendingRevision) return 'pending';
		return status.worker.lastDeployedRevision ? 'upToDate' : 'neverDeployed';
	}
	if (!status.server) return liveChecked ? 'unavailable' : 'checking';
	const server = status.server;
	if (server.modsRevision == null) return 'neverDeployed';
	const lastOperation = server.lastOperation;
	if (
		lastOperation?.status === 'succeeded' &&
		lastOperation.modsRevision != null &&
		lastOperation.publicationRevision != null &&
		status.publicationRevision != null &&
		lastOperation.publicationRevision >= status.publicationRevision
	) {
		return 'upToDate';
	}
	if (
		lastOperation?.modsRevision != null &&
		status.publicationRevision != null &&
		lastOperation.publicationRevision != null &&
		lastOperation.publicationRevision < status.publicationRevision
	) {
		return 'pending';
	}
	return 'deployed';
}

export class ServerSync {
	/// Last worker-mode status this game+profile observed — polled by the
	/// navbar and updated by the remote tab whenever it loads status.
	status = $state<ServerSyncStatus | null>(null);

	/// What the navbar badge should show. Only worker observations ever
	/// reach `status`, so a manual-sync profile correctly shows no
	/// remote dot.
	remoteBadge = $derived.by<'upToDate' | 'pending' | null>(() => {
		const state = remoteDeployState(this.status);
		if (state === 'upToDate') return 'upToDate';
		if (state === 'pending') return 'pending';
		return null;
	});

	#key: string | null = null;
	#pollTimer: ReturnType<typeof setInterval> | null = null;
	#pollInFlight: number | null = null;
	/// Bumped whenever the polling target changes — an in-flight request
	/// from an older generation is ignored on success and failure alike.
	#generation = 0;

	/// Identifies the active game+profile the badge belongs to, or null
	/// when the game has no dedicated server or no profile is selected.
	currentSyncKey(): string | null {
		const game = games.active;
		if (game?.dedicatedServer == null || profiles.activeId == null) return null;
		return `${game.slug}:${profiles.activeId}`;
	}

	/// Called when the active game or profile may have changed. A
	/// different key drops the old profile's status; the same key keeps
	/// it and only re-evaluates whether polling should run.
	start(key: string | null) {
		if (key !== this.#key) {
			this.#key = key;
			this.status = null;
		}
		void this.reconfigure();
	}

	/// Re-reads the settings for the current key and starts or stops
	/// polling. The observed status is only dropped when the profile is
	/// no longer a remote worker setup.
	async reconfigure() {
		const generation = ++this.#generation;
		this.stop();
		const key = this.#key;
		if (key == null) {
			this.status = null;
			return;
		}
		try {
			const settings = await api.profile.server.getSettings({ quiet: true });
			if (generation !== this.#generation || key !== this.#key) return;
			if (settings == null || !this.#enableIfWorker(settings)) {
				this.status = null;
				return;
			}
			void this.#poll(key);
			this.#pollTimer = setInterval(() => void this.#poll(key), 60_000);
		} catch {
			// Settings could not be read — nothing reliable to poll for.
		}
	}

	stop() {
		if (this.#pollTimer) clearInterval(this.#pollTimer);
		this.#pollTimer = null;
	}

	#enableIfWorker(settings: ProfileServerSettings) {
		return settings.remote.syncMode === 'worker' && settings.remote.host.trim() !== '';
	}

	async #poll(key: string) {
		const generation = this.#generation;
		// Only a poll of the current generation blocks another one — a
		// stale request must not delay the new target's first poll.
		if (this.#pollInFlight === generation) return;
		this.#pollInFlight = generation;
		try {
			const status = await api.profile.server.getSyncStatus(false, '', '', {
				quiet: true
			});
			// The target moved while the request was in flight — this
			// answer belongs to a different profile.
			if (generation !== this.#generation || key !== this.#key) return;
			// Anything but a worker observation cannot describe the badge —
			// keep the last known state.
			if (!this.#isWorkerObservation(status)) return;
			this.status = status;
		} catch {
			// A failed poll keeps the last known status — the navbar dot
			// must not flap on a transient error.
		} finally {
			if (this.#pollInFlight === generation) this.#pollInFlight = null;
		}
	}

	#isWorkerObservation(status: ServerSyncStatus) {
		return status.mode === 'worker' && status.worker != null;
	}

	/// The remote tab feeds the statuses it actually applied here so the
	/// navbar reflects what the page shows.
	record(key: string | null, status: ServerSyncStatus | null) {
		if (key !== this.#key || status == null || !this.#isWorkerObservation(status)) return;
		this.status = status;
	}
}

const serverSync = new ServerSync();
export default serverSync;
