import * as api from '$lib/api';
import type { ServerSyncStatus } from '$lib/types';

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

class ServerSyncState {
	status: ServerSyncStatus | null = $state(null);
	pending = $derived(remoteDeployState(this.status) === 'pending');

	#profileId: number | null = null;
	#timer: ReturnType<typeof setInterval> | null = null;

	/// Records a status fetched by the server page so the navbar badge
	/// reflects what the user sees there. `null` clears it.
	record(status: ServerSyncStatus | null) {
		this.status = status;
	}

	/// Starts (or retargets) the cheap background status poll. Only
	/// remote+worker configurations can be polled without opening a
	/// transport session, so other setups just reset the state.
	start(profileId: number | null) {
		this.stop();
		if (profileId === null) return;
		this.#profileId = profileId;
		void this.#enableIfWorker(profileId);
	}

	stop() {
		this.#profileId = null;
		if (this.#timer) clearInterval(this.#timer);
		this.#timer = null;
		this.status = null;
	}

	async #enableIfWorker(profileId: number) {
		try {
			const settings = await api.profile.server.getSettings();
			if (
				this.#profileId !== profileId ||
				settings?.remote.syncMode !== 'worker' ||
				settings.remote.host.trim() === ''
			) {
				return;
			}
			void this.#poll(profileId);
			this.#timer = setInterval(() => void this.#poll(profileId), 60_000);
		} catch {
			// Polling is best-effort; the badge simply stays unset.
		}
	}

	async #poll(profileId: number) {
		try {
			this.status = await api.profile.server.getSyncStatus(false, '', '');
		} catch {
			if (this.#profileId === profileId) this.status = null;
		}
	}
}

const serverSync = new ServerSyncState();

export default serverSync;
