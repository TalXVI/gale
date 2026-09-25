import * as api from '$lib/api';
import type {
	LocalWorkerStatus,
	PendingPublication,
	ProfileServerSettings,
	RemoteServerSettings,
	RemoteProtocol,
	RestartPolicy,
	SavedServerCredentials
} from '$lib/types';
import games from '$lib/state/game.svelte';
import profiles from '$lib/state/profile.svelte';
import serverSync from '$lib/state/serverSync.svelte';
import { confirm, message, open as openDialog } from '@tauri-apps/plugin-dialog';
import { m } from '$lib/paraglide/messages';
import { pushInfoToast } from '$lib/toast';

export const DEFAULT_SFTP_PORT = '22';
export const DEFAULT_FTP_PORT = '21';
const FALLBACK_SERVER_PORT = 2456;
const MAX_PORT = 65535;

export type SyncChoice = 'local' | 'hostedWorker' | 'worker';

/// Initial settings built from the active game, for profiles that have
/// never configured a dedicated server.
export function defaultSettings(): ProfileServerSettings {
	const game = games.active;
	return {
		location: 'local',
		serverName: game ? `${game.name} Server` : 'Dedicated Server',
		world: game?.slug === 'valheim' ? 'Dedicated' : '',
		port: game?.dedicatedServer?.defaultPort || FALLBACK_SERVER_PORT,
		publicServer: true,
		crossplay: false,
		extraArgs: '',
		remote: {
			protocol: 'sftp',
			host: '',
			port: Number(DEFAULT_SFTP_PORT),
			username: '',
			serverDirectory: '/',
			authentication: 'password',
			privateKeyPath: '',
			trustedHostKey: null,
			trustedCertificate: null,
			syncMode: 'local',
			worker: { address: '', hosted: false, autoSync: false, autoMods: false },
			hostControl: { provider: 'none', datHostServerId: '', datHostUsername: '' },
			restartPolicy: 'manual'
		}
	};
}

export function parsePort(value: string, label: string) {
	const parsed = Number(value);
	if (!Number.isInteger(parsed) || parsed < 1 || parsed > MAX_PORT)
		throw new Error(m.dedicatedServerDialog_portError({ label }));
	return parsed;
}

/// All editable server settings, credentials-in-flight, and the
/// save/discard bookkeeping for the dedicated-server page. The form
/// fields are shared between the local and remote tabs; secret inputs
/// never persist — they exist only until a save or discard.
export class ServerFormState {
	form = $state<ProfileServerSettings>(defaultSettings());
	port = $state('');
	remotePort = $state(DEFAULT_SFTP_PORT);
	/// The UI-level sync choice: 'hostedWorker' maps to syncMode 'worker'
	/// with `hosted: true`.
	syncChoice = $state<SyncChoice>('local');
	localWorker = $state<LocalWorkerStatus | null>(null);
	/// The automation state the worker last confirmed (for the managed
	/// worker, what it reports live). A failed save snaps the controls
	/// back to this instead of leaving intent that never took effect.
	savedAutomation = $state<{
		autoSync: boolean;
		autoMods: boolean;
		restartPolicy: RestartPolicy;
	}>({ autoSync: false, autoMods: false, restartPolicy: 'manual' });
	gamePassword = $state('');
	remotePassword = $state('');
	workerToken = $state('');
	datHostPassword = $state('');
	rememberGamePassword = $state(true);
	rememberRemotePassword = $state(true);
	savedCredentials = $state<SavedServerCredentials | null>(null);
	hasSavedSettings = $state(false);
	loadingSettings = $state(false);
	saving = $state(false);
	launching = $state(false);
	testing = $state(false);
	testingWorker = $state(false);
	provisioning = $state(false);
	workerBusy = $state(false);
	busy = $derived(
		this.saving ||
			this.launching ||
			this.testing ||
			this.testingWorker ||
			this.provisioning ||
			this.workerBusy
	);

	#savedJson = $state('');
	/// The tab binding (`location`) is just the last-viewed tab now, so it
	/// is normalized out of the comparison; the remember checkboxes stay
	/// in because toggling them controls whether secrets persist.
	#liveJson = $derived(
		JSON.stringify({
			form: { ...this.form, location: 'local' },
			port: this.port,
			remotePort: this.remotePort,
			syncChoice: this.syncChoice,
			rememberGamePassword: this.rememberGamePassword,
			rememberRemotePassword: this.rememberRemotePassword
		})
	);
	secretsDirty = $derived(
		this.gamePassword !== '' ||
			this.remotePassword !== '' ||
			this.workerToken !== '' ||
			this.datHostPassword !== ''
	);
	/// Transport-relevant form drift — a typed secret does not count, so
	/// the user can still preview and deploy with an unsaved credential.
	settingsChanged = $derived(this.#savedJson !== '' && this.#liveJson !== this.#savedJson);
	dirty = $derived(
		this.#savedJson !== '' && (this.#liveJson !== this.#savedJson || this.secretsDirty)
	);

	/// The saved settings decide whether the remote panels mount at all —
	/// unsaved edits never enable or disable the status/deploy surface.
	remoteConfigured = $derived.by(() => {
		if (!this.hasSavedSettings || !this.#savedJson) return false;
		const saved = JSON.parse(this.#savedJson) as {
			form: ProfileServerSettings;
			syncChoice: SyncChoice;
		};
		const remote = saved.form.remote;
		if (remote.host.trim() === '') return false;
		return (
			saved.syncChoice === 'local' ||
			saved.syncChoice === 'hostedWorker' ||
			remote.worker.address.trim() !== ''
		);
	});

	/// Whether the remote password/passphrase field should show the
	/// "saved" marker for the currently selected authentication.
	remoteCredentialSaved = $derived.by(() => {
		const saved = this.savedCredentials;
		if (!saved) return false;
		const remote = this.form.remote;
		if (remote.protocol === 'sftp') {
			if (remote.authentication === 'agent') return false;
			return remote.authentication === 'password' ? saved.sftpPassword : saved.sshKeyPassphrase;
		}
		return saved.ftpPassword;
	});

	async load() {
		this.loadingSettings = true;
		try {
			const value = await api.profile.server.getSettings();
			this.hasSavedSettings = value !== null;
			this.form = value ?? defaultSettings();
			this.port = String(
				this.form.port || games.active?.dedicatedServer?.defaultPort || FALLBACK_SERVER_PORT
			);
			this.remotePort = String(
				this.form.remote.port ||
					(this.form.remote.protocol === 'sftp' ? DEFAULT_SFTP_PORT : DEFAULT_FTP_PORT)
			);
			this.syncChoice =
				this.form.remote.syncMode === 'worker'
					? this.form.remote.worker.hosted
						? 'hostedWorker'
						: 'worker'
					: 'local';
			await this.refreshLocalWorker();
			// The worker's journal is the authoritative automation state —
			// for the managed worker its live report wins over the stored
			// copy, which only seeds new installs.
			const liveWorker = this.syncChoice === 'hostedWorker' ? this.localWorker?.worker : null;
			this.form.remote.worker.autoSync = liveWorker?.autoSync ?? this.form.remote.worker.autoSync;
			this.form.remote.worker.autoMods = liveWorker?.autoMods ?? this.form.remote.worker.autoMods;
			this.form.remote.restartPolicy = liveWorker?.restartPolicy ?? this.form.remote.restartPolicy;
			this.savedAutomation = {
				autoSync: this.form.remote.worker.autoSync,
				autoMods: this.form.remote.worker.autoMods,
				restartPolicy: this.form.remote.restartPolicy
			};
			await this.refreshSavedCredentials();
			this.clearSecrets();
			this.#savedJson = this.#liveJson;
		} finally {
			this.loadingSettings = false;
		}
	}

	clearSecrets() {
		this.gamePassword = '';
		this.remotePassword = '';
		this.workerToken = '';
		this.datHostPassword = '';
	}

	async refreshSavedCredentials() {
		try {
			this.savedCredentials = await api.profile.server.getSavedCredentials();
		} catch {
			this.savedCredentials = null;
		}
	}

	async refreshLocalWorker() {
		try {
			this.localWorker = await api.profile.server.getLocalWorkerStatus();
			// An unfinished setup would otherwise be invisible: the profile
			// still reads 'local' because linking it is the step that
			// failed. Show the hosted-worker section so Finish setup is
			// one click away.
			if (this.localWorker?.ownership === 'incomplete' && this.syncChoice === 'local') {
				this.syncChoice = 'hostedWorker';
			}
		} catch {
			this.localWorker = null;
		}
	}

	remoteSettings(): RemoteServerSettings {
		return {
			...this.form.remote,
			host: this.form.remote.host.trim(),
			port: parsePort(
				this.remotePort,
				this.form.remote.protocol === 'sftp'
					? m.dedicatedServerDialog_sshPort()
					: m.dedicatedServerDialog_ftpPort()
			),
			username: this.form.remote.username.trim(),
			serverDirectory: this.form.remote.serverDirectory.trim(),
			privateKeyPath: this.form.remote.privateKeyPath.trim(),
			syncMode: this.syncChoice === 'local' ? 'local' : 'worker',
			worker: {
				...this.form.remote.worker,
				address: this.form.remote.worker.address.trim(),
				hosted: this.syncChoice === 'hostedWorker'
			},
			hostControl: {
				...this.form.remote.hostControl,
				datHostServerId: this.form.remote.hostControl.datHostServerId.trim(),
				datHostUsername: this.form.remote.hostControl.datHostUsername.trim()
			}
		};
	}

	changeRemoteProtocol(value: RemoteProtocol) {
		if (
			(this.form.remote.protocol === 'sftp' && this.remotePort === DEFAULT_SFTP_PORT) ||
			(this.form.remote.protocol !== 'sftp' && this.remotePort === DEFAULT_FTP_PORT)
		) {
			this.remotePort = value === 'sftp' ? DEFAULT_SFTP_PORT : DEFAULT_FTP_PORT;
		}
		this.form.remote.protocol = value;
		this.form.remote.trustedHostKey = null;
		this.form.remote.trustedCertificate = null;
	}

	async choosePrivateKey() {
		const selected = await openDialog({
			title: m.dedicatedServerDialog_privateKeyTitle(),
			directory: false,
			multiple: false
		});
		if (typeof selected === 'string') this.form.remote.privateKeyPath = selected;
	}

	settings(): ProfileServerSettings {
		return {
			...this.form,
			serverName: this.form.serverName.trim(),
			world: this.form.world.trim(),
			port: parsePort(this.port, m.dedicatedServerDialog_serverPort()),
			extraArgs: this.form.extraArgs.trim(),
			remote: this.remoteSettings()
		};
	}

	async checkedSettings() {
		try {
			return this.settings();
		} catch (error) {
			await message(error instanceof Error ? error.message : String(error));
			return null;
		}
	}

	async trustHost(fingerprint: string) {
		const accepted = await confirm(m.dedicatedServerDialog_trustMessage({ fingerprint }), {
			title: m.dedicatedServerDialog_trustTitle(),
			kind: 'warning'
		});
		if (accepted) this.form.remote.trustedHostKey = fingerprint;
		return accepted;
	}

	async trustInvalidCertificate(fingerprint: string) {
		const accepted = await confirm(
			m.dedicatedServerDialog_certificateMessage({ host: this.form.remote.host.trim() }),
			{
				title: m.dedicatedServerDialog_certificateTitle(),
				kind: 'warning'
			}
		);
		if (accepted) this.form.remote.trustedCertificate = fingerprint;
		return accepted;
	}

	async testConnection() {
		const current = await this.checkedSettings();
		if (!current) return;
		this.testing = true;
		try {
			let result = await api.profile.server.testRemoteConnection(
				current.remote,
				this.remotePassword
			);
			if (result.status === 'hostKeyUntrusted') {
				if (!(await this.trustHost(result.fingerprint))) return;
				current.remote.trustedHostKey = this.form.remote.trustedHostKey;
				result = await api.profile.server.testRemoteConnection(current.remote, this.remotePassword);
			}
			if (result.status === 'certificateUntrusted') {
				if (!(await this.trustInvalidCertificate(result.fingerprint))) return;
				current.remote.trustedCertificate = this.form.remote.trustedCertificate;
				result = await api.profile.server.testRemoteConnection(current.remote, this.remotePassword);
			}
			if (result.status !== 'connected') return;
			await message(
				!result.encrypted
					? m.dedicatedServerDialog_connectionPlain({ host: this.form.remote.host })
					: this.form.remote.protocol !== 'sftp' && this.form.remote.trustedCertificate
						? m.dedicatedServerDialog_connectionEncrypted({ host: this.form.remote.host })
						: m.dedicatedServerDialog_connectionSecure({ host: this.form.remote.host }),
				{
					title: m.dedicatedServerDialog_connectionTitle(),
					kind: 'info'
				}
			);
		} finally {
			this.testing = false;
		}
	}

	async testWorker() {
		const current = await this.checkedSettings();
		if (!current) return;
		this.testingWorker = true;
		try {
			const status = await api.profile.server.testWorkerConnection(
				current.remote,
				this.workerToken
			);
			await message(
				m.dedicatedServerDialog_workerConnected({
					workerId: status.workerId,
					autoSync: status.autoSync
						? m.dedicatedServerDialog_workerAutoOn()
						: m.dedicatedServerDialog_workerAutoOff()
				}),
				{ title: m.dedicatedServerDialog_connectionTitle(), kind: 'info' }
			);
		} finally {
			this.testingWorker = false;
		}
	}

	async save() {
		const current = await this.checkedSettings();
		if (!current) return;
		this.saving = true;
		try {
			await api.profile.server.setSettings(
				current,
				this.remotePassword,
				this.workerToken,
				this.datHostPassword,
				this.rememberRemotePassword,
				this.gamePassword,
				this.rememberGamePassword
			);
			// Automation toggles may have changed — re-read the worker's own
			// state so the pending banner reflects what it will actually do.
			await this.refreshLocalWorker();
			this.savedAutomation = {
				autoSync: this.form.remote.worker.autoSync,
				autoMods: this.form.remote.worker.autoMods,
				restartPolicy: this.form.remote.restartPolicy
			};
			this.form = current;
			this.hasSavedSettings = true;
			this.clearSecrets();
			await this.refreshSavedCredentials();
			this.#savedJson = this.#liveJson;
			// The navbar poll targets remote+worker setups; a save may have
			// just created or removed one.
			serverSync.start(profiles.activeId);
			pushInfoToast({ message: m.dedicatedServerDialog_saved() });
		} catch {
			// The save failed — the backend leaves stored settings
			// untouched when the worker did not accept the change. Snap
			// the automation controls back to the worker's actual state.
			await this.refreshLocalWorker();
			this.reconcileAutomation();
		} finally {
			this.saving = false;
		}
	}

	/// Restores the last loaded/saved values and clears typed credentials.
	/// The current tab survives — it is not a saved value anymore.
	discard() {
		this.clearSecrets();
		if (!this.#savedJson) return;
		const saved = JSON.parse(this.#savedJson) as {
			form: ProfileServerSettings;
			port: string;
			remotePort: string;
			syncChoice: SyncChoice;
			rememberGamePassword: boolean;
			rememberRemotePassword: boolean;
		};
		this.form = { ...saved.form, location: this.form.location };
		this.port = saved.port;
		this.remotePort = saved.remotePort;
		this.syncChoice = saved.syncChoice;
		this.rememberGamePassword = saved.rememberGamePassword;
		this.rememberRemotePassword = saved.rememberRemotePassword;
	}

	/// Reverts the automation controls to the last state the worker
	/// confirmed — its live report for the managed worker, else the last
	/// values it acknowledged.
	reconcileAutomation() {
		const liveWorker = this.syncChoice === 'hostedWorker' ? this.localWorker?.worker : null;
		this.form.remote.worker.autoSync = liveWorker?.autoSync ?? this.savedAutomation.autoSync;
		this.form.remote.worker.autoMods = liveWorker?.autoMods ?? this.savedAutomation.autoMods;
		this.form.remote.restartPolicy =
			liveWorker?.restartPolicy ?? this.savedAutomation.restartPolicy;
	}

	/// Saves the current transport settings first — provisioning derives
	/// the worker's config from the *saved* settings, so unsaved edits
	/// would otherwise leave worker and desktop pointing at different
	/// remotes.
	///
	/// The save uses `local` sync mode on purpose: a fresh profile has no
	/// worker address yet, so `worker` mode would fail validation before
	/// provisioning could start. Once the service is installed, the
	/// backend itself persists hosted-worker mode with the loopback
	/// address; the page then mirrors that state.
	async provisionWorker() {
		const current = await this.checkedSettings();
		if (!current) return;
		current.remote.syncMode = 'local';
		current.remote.worker.hosted = false;
		this.provisioning = true;
		try {
			await api.profile.server.setSettings(
				current,
				this.remotePassword,
				this.workerToken,
				this.datHostPassword,
				this.rememberRemotePassword,
				this.gamePassword,
				this.rememberGamePassword
			);
			this.localWorker = await api.profile.server.provisionLocalWorker(
				this.remotePassword,
				this.datHostPassword
			);
			if (this.localWorker.binding)
				this.form.remote.worker.address = this.localWorker.binding.address;
			this.syncChoice = 'hostedWorker';
			// A reprovisioned worker keeps its journal — adopt whatever
			// automation flags it actually runs.
			this.reconcileAutomation();
			this.savedAutomation = {
				autoSync: this.form.remote.worker.autoSync,
				autoMods: this.form.remote.worker.autoMods,
				restartPolicy: this.form.remote.restartPolicy
			};
			// Provisioning persisted the remote settings itself (hosted
			// worker mode plus the loopback address), so the saved baseline
			// moves to what the page now shows.
			this.hasSavedSettings = true;
			this.clearSecrets();
			await this.refreshSavedCredentials();
			this.#savedJson = this.#liveJson;
		} finally {
			this.provisioning = false;
		}
	}

	async controlWorker(action: 'start' | 'stop' | 'restart') {
		this.workerBusy = true;
		try {
			this.localWorker = await api.profile.server.controlLocalWorker(action);
		} finally {
			this.workerBusy = false;
		}
	}

	async updateWorker() {
		this.workerBusy = true;
		try {
			this.localWorker = await api.profile.server.updateLocalWorker();
		} finally {
			this.workerBusy = false;
		}
	}

	async uninstallWorker() {
		const accepted = await confirm(m.dedicatedServerDialog_localWorkerUninstallConfirm(), {
			title: m.dedicatedServerDialog_localWorkerUninstall(),
			kind: 'warning'
		});
		if (!accepted) return;
		this.workerBusy = true;
		try {
			this.localWorker = await api.profile.server.uninstallLocalWorker();
			this.syncChoice = 'local';
			this.form.remote.worker.address = '';
			// The backend reverted the profile to Local sync — patch the
			// saved baseline the same way so other unsaved edits stay dirty.
			if (this.#savedJson) {
				const saved = JSON.parse(this.#savedJson);
				saved.syncChoice = 'local';
				saved.form.remote.worker.address = '';
				this.#savedJson = JSON.stringify(saved);
			}
		} finally {
			this.workerBusy = false;
		}
	}

	/// The pending banner mirrors the worker's own automation state —
	/// never the unsaved checkboxes — so it only promises an automatic
	/// deployment the worker can actually perform.
	pendingLabel(pending: PendingPublication): string {
		switch (pending.mode) {
			case 'automatic':
				return pending.retrying
					? m.dedicatedServerDialog_localWorkerPendingRetry()
					: m.dedicatedServerDialog_localWorkerPending();
			case 'modsManual':
				return m.dedicatedServerDialog_localWorkerPendingModsManual();
			case 'manual':
				return m.dedicatedServerDialog_localWorkerPendingManual();
		}
	}

	localWorkerStateLabel(state: string | undefined): string {
		switch (state) {
			case 'running':
				return m.dedicatedServerDialog_localWorkerStateRunning();
			case 'stopped':
				return m.dedicatedServerDialog_localWorkerStateStopped();
			case 'startPending':
				return m.dedicatedServerDialog_localWorkerStateStartPending();
			case 'stopPending':
				return m.dedicatedServerDialog_localWorkerStateStopPending();
			case 'notInstalled':
				return m.dedicatedServerDialog_localWorkerStateNotInstalled();
			default:
				return m.dedicatedServerDialog_localWorkerStateOther();
		}
	}

	async launch() {
		const current = await this.checkedSettings();
		if (!current) return;
		this.launching = true;
		try {
			await api.profile.server.launch(current, this.gamePassword, this.rememberGamePassword);
			pushInfoToast({ message: m.toolBar_launchServer_started() });
		} finally {
			this.launching = false;
		}
	}
}
