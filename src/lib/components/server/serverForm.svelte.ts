import * as api from '$lib/api';
import type { ServerCredentials } from '$lib/api/profile/server';
import type {
	LocalWorkerStatus,
	ProfileServerSettings,
	RemoteServerSettings,
	RemoteProtocol,
	SavedServerCredentials,
	TransportSettings,
	WorkerStatus
} from '$lib/types';
import games from '$lib/state/game.svelte';
import serverSync from '$lib/state/serverSync.svelte';
import { confirm, message, open as openDialog } from '@tauri-apps/plugin-dialog';
import { m } from '$lib/paraglide/messages';
import { pushInfoToast } from '$lib/toast';

export const DEFAULT_SFTP_PORT = '22';
export const DEFAULT_FTP_PORT = '21';
const FALLBACK_SERVER_PORT = 2456;
const MAX_PORT = 65535;

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
			transport: {
				protocol: 'sftp',
				host: '',
				port: Number(DEFAULT_SFTP_PORT),
				username: '',
				serverDirectory: '/',
				authentication: 'password',
				privateKeyPath: '',
				trustedHostKey: null,
				trustedCertificate: null
			},
			executor: { mode: 'local', workerAddress: '' },
			hostControl: { provider: 'none', datHostServerId: '', datHostUsername: '' },
			automation: { autoDeployMods: false, restartPolicy: 'manual' }
		}
	};
}

/// Empty typed credentials, each remembered by default.
function emptyCredentials(): ServerCredentials {
	const input = () => ({ value: '', remember: true });
	return {
		gamePassword: input(),
		remotePassword: input(),
		workerToken: input(),
		datHostPassword: input()
	};
}

/// What the unsaved-changes check compares: everything a save persists
/// except typed secret values. The tab binding (`location`) is just the
/// last-viewed tab, so it is normalized out; the remember choices stay in
/// because toggling them controls whether secrets persist.
type FormBaseline = {
	form: ProfileServerSettings;
	port: string;
	remotePort: string;
	remember: Record<keyof ServerCredentials, boolean>;
};

export function parsePort(value: string, label: string) {
	const parsed = Number(value);
	if (!Number.isInteger(parsed) || parsed < 1 || parsed > MAX_PORT)
		throw new Error(m.dedicatedServerDialog_portError({ label }));
	return parsed;
}

/// All editable server settings, credentials-in-flight, and the
/// save/discard bookkeeping for the dedicated-server page. The form
/// fields are shared between the local and remote tabs; secret inputs
/// never persist. They exist only until a save or discard.
export class ServerFormState {
	form = $state<ProfileServerSettings>(defaultSettings());
	port = $state('');
	remotePort = $state(DEFAULT_SFTP_PORT);
	localWorker = $state<LocalWorkerStatus | null>(null);
	/// Typed credentials and their remember choices.
	credentials = $state<ServerCredentials>(emptyCredentials());
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

	/// The last loaded or saved values; `null` until the first load.
	#baseline = $state.raw<FormBaseline | null>(null);
	#baselineJson = $derived(this.#baseline && JSON.stringify(this.#baseline));
	#loadSeq = 0;
	#live = $derived.by(
		(): FormBaseline => ({
			form: { ...this.form, location: 'local' },
			port: this.port,
			remotePort: this.remotePort,
			remember: {
				gamePassword: this.credentials.gamePassword.remember,
				remotePassword: this.credentials.remotePassword.remember,
				workerToken: this.credentials.workerToken.remember,
				datHostPassword: this.credentials.datHostPassword.remember
			}
		})
	);
	secretsDirty = $derived(Object.values(this.credentials).some((input) => input.value !== ''));
	/// Transport-relevant form drift. A typed secret does not count, so
	/// the user can still preview and deploy with an unsaved credential.
	settingsChanged = $derived(
		this.#baselineJson !== null && JSON.stringify(this.#live) !== this.#baselineJson
	);
	dirty = $derived(this.settingsChanged || (this.#baseline !== null && this.secretsDirty));

	/// The saved settings decide whether the remote panels mount at all.
	/// Unsaved edits never enable or disable the status/deploy panel.
	remoteConfigured = $derived.by(() => {
		const saved = this.#baseline;
		if (!this.hasSavedSettings || !saved) return false;
		const remote = saved.form.remote;
		if (remote.transport.host.trim() === '') return false;
		return remote.executor.mode !== 'worker' || remote.executor.workerAddress.trim() !== '';
	});

	/// Whether the remote password/passphrase field should show the
	/// "saved" marker for the currently selected authentication.
	remoteCredentialSaved = $derived.by(() => {
		const saved = this.savedCredentials;
		if (!saved) return false;
		const remote = this.form.remote.transport;
		if (remote.protocol === 'sftp') {
			if (remote.authentication === 'agent') return false;
			return remote.authentication === 'password' ? saved.sftpPassword : saved.sshKeyPassphrase;
		}
		return saved.ftpPassword;
	});

	async load() {
		const seq = ++this.#loadSeq;
		this.loadingSettings = true;
		try {
			const [value, localWorker, credentials] = await Promise.all([
				api.profile.server.getSettings(),
				api.profile.server.getLocalWorkerStatus().catch(() => null),
				api.profile.server.getSavedCredentials().catch(() => null)
			]);
			if (seq !== this.#loadSeq) return;
			this.hasSavedSettings = value !== null;
			this.form = value ?? defaultSettings();
			this.port = String(
				this.form.port || games.active?.dedicatedServer?.defaultPort || FALLBACK_SERVER_PORT
			);
			const transport = this.form.remote.transport;
			this.remotePort = String(
				transport.port || (transport.protocol === 'sftp' ? DEFAULT_SFTP_PORT : DEFAULT_FTP_PORT)
			);
			this.localWorker = localWorker;
			const executor = this.form.remote.executor;
			if (localWorker?.ownership === 'incomplete' && executor.mode === 'local') {
				executor.mode = 'hostedWorker';
			}
			// The worker's journal holds the automation state. For the managed
			// worker, its live report wins over the stored
			// copy, which only seeds new installs.
			const liveWorker = executor.mode === 'hostedWorker' ? this.localWorker?.worker : null;
			const automation = this.form.remote.automation;
			automation.autoDeployMods = liveWorker?.autoDeployMods ?? automation.autoDeployMods;
			automation.restartPolicy = liveWorker?.restartPolicy ?? automation.restartPolicy;
			this.savedCredentials = credentials;
			this.clearSecrets();
			this.#commitBaseline();
		} finally {
			if (seq === this.#loadSeq) this.loadingSettings = false;
		}
	}

	clearSecrets() {
		for (const input of Object.values(this.credentials)) input.value = '';
	}

	/// Makes what the page shows now the saved state.
	#commitBaseline() {
		this.#baseline = $state.snapshot(this.#live);
	}

	async refreshSavedCredentials() {
		try {
			this.savedCredentials = await api.profile.server.getSavedCredentials();
		} catch {
			this.savedCredentials = null;
		}
	}

	async refreshLocalWorker({ background = false } = {}) {
		try {
			const status = await api.profile.server.getLocalWorkerStatus(
				background ? { quiet: true } : undefined
			);
			this.localWorker = status;
			// An unfinished setup would otherwise be invisible: the profile
			// still reads 'local' because linking it is the step that
			// failed. Show the hosted-worker section so Finish setup is
			// one click away.
			const executor = this.form.remote.executor;
			if (status?.ownership === 'incomplete' && executor.mode === 'local') {
				executor.mode = 'hostedWorker';
			}
		} catch {
			// A background refresh keeps the last known service state.
			// Only a foreground call reports the worker as unknown.
			if (!background) this.localWorker = null;
		}
	}

	remoteSettings(): RemoteServerSettings {
		const transport: TransportSettings = this.form.remote.transport;
		return {
			...this.form.remote,
			transport: {
				...transport,
				host: transport.host.trim(),
				port: parsePort(
					this.remotePort,
					transport.protocol === 'sftp'
						? m.dedicatedServerDialog_sshPort()
						: m.dedicatedServerDialog_ftpPort()
				),
				username: transport.username.trim(),
				serverDirectory: transport.serverDirectory.trim(),
				privateKeyPath: transport.privateKeyPath.trim()
			},
			executor: {
				...this.form.remote.executor,
				workerAddress: this.form.remote.executor.workerAddress.trim()
			},
			hostControl: {
				...this.form.remote.hostControl,
				datHostServerId: this.form.remote.hostControl.datHostServerId.trim(),
				datHostUsername: this.form.remote.hostControl.datHostUsername.trim()
			},
			automation: { ...this.form.remote.automation }
		};
	}

	changeRemoteProtocol(value: RemoteProtocol) {
		const transport = this.form.remote.transport;
		if (
			(transport.protocol === 'sftp' && this.remotePort === DEFAULT_SFTP_PORT) ||
			(transport.protocol !== 'sftp' && this.remotePort === DEFAULT_FTP_PORT)
		) {
			this.remotePort = value === 'sftp' ? DEFAULT_SFTP_PORT : DEFAULT_FTP_PORT;
		}
		transport.protocol = value;
		transport.trustedHostKey = null;
		transport.trustedCertificate = null;
	}

	async choosePrivateKey() {
		const selected = await openDialog({
			title: m.dedicatedServerDialog_privateKeyTitle(),
			directory: false,
			multiple: false
		});
		if (typeof selected === 'string') this.form.remote.transport.privateKeyPath = selected;
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
		if (accepted) this.form.remote.transport.trustedHostKey = fingerprint;
		return accepted;
	}

	async trustInvalidCertificate(fingerprint: string) {
		const accepted = await confirm(
			m.dedicatedServerDialog_certificateMessage({
				host: this.form.remote.transport.host.trim(),
				fingerprint
			}),
			{
				title: m.dedicatedServerDialog_certificateTitle(),
				kind: 'warning'
			}
		);
		if (accepted) this.form.remote.transport.trustedCertificate = fingerprint;
		return accepted;
	}

	async testConnection() {
		const current = await this.checkedSettings();
		if (!current) return;
		this.testing = true;
		try {
			const password = this.credentials.remotePassword.value;
			let result = await api.profile.server.testRemoteConnection(current.remote, password);
			if (result.status === 'hostKeyUntrusted') {
				if (!(await this.trustHost(result.fingerprint))) return;
				current.remote.transport.trustedHostKey = this.form.remote.transport.trustedHostKey;
				result = await api.profile.server.testRemoteConnection(current.remote, password);
			}
			if (result.status === 'certificateUntrusted') {
				if (!(await this.trustInvalidCertificate(result.fingerprint))) return;
				current.remote.transport.trustedCertificate = this.form.remote.transport.trustedCertificate;
				result = await api.profile.server.testRemoteConnection(current.remote, password);
			}
			if (result.status !== 'connected') return;
			const { host, protocol, trustedCertificate } = this.form.remote.transport;
			await message(
				!result.encrypted
					? m.dedicatedServerDialog_connectionPlain({ host })
					: protocol !== 'sftp' && trustedCertificate
						? m.dedicatedServerDialog_connectionEncrypted({ host })
						: m.dedicatedServerDialog_connectionSecure({ host }),
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
				this.credentials.workerToken.value
			);
			await message(
				m.dedicatedServerDialog_workerConnected({
					workerId: status.workerId,
					autoDeployMods: status.autoDeployMods
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
			await api.profile.server.setSettings(current, $state.snapshot(this.credentials));
			// Automation may have changed. Re-read the worker's own
			// state so the pending banner reflects what it will actually do.
			await this.refreshLocalWorker();
			this.form = current;
			this.hasSavedSettings = true;
			this.clearSecrets();
			await this.refreshSavedCredentials();
			this.#commitBaseline();
			// The navbar poll targets remote+worker setups; a save may have
			// just created or removed one.
			void serverSync.reconfigure();
			pushInfoToast({ message: m.dedicatedServerDialog_saved() });
		} catch {
			// The save failed. The backend leaves stored settings
			// untouched when the worker did not accept the change. Snap
			// the automation controls back to the worker's actual state.
			await this.refreshLocalWorker();
			this.reconcileAutomation();
		} finally {
			this.saving = false;
		}
	}

	/// Restores the last loaded/saved values and clears typed credentials.
	/// The current tab survives. It is not a saved value anymore.
	discard() {
		this.clearSecrets();
		const saved = this.#baseline;
		if (!saved) return;
		// A copy, so later edits never reach the baseline.
		this.form = { ...structuredClone(saved.form), location: this.form.location };
		this.port = saved.port;
		this.remotePort = saved.remotePort;
		for (const key of Object.keys(saved.remember) as (keyof ServerCredentials)[]) {
			this.credentials[key].remember = saved.remember[key];
		}
	}

	/// Reverts the automation controls to the managed worker's live report
	/// or the last values saved for this profile.
	reconcileAutomation() {
		const liveWorker =
			this.form.remote.executor.mode === 'hostedWorker' ? this.localWorker?.worker : null;
		const saved = (this.#baseline?.form.remote ?? this.form.remote).automation;
		this.form.remote.automation = {
			autoDeployMods: liveWorker?.autoDeployMods ?? saved.autoDeployMods,
			restartPolicy: liveWorker?.restartPolicy ?? saved.restartPolicy
		};
	}

	/// Sets the worker up from the page as it stands, saved or not. Once
	/// the service is installed, the backend saves these settings in
	/// hosted-worker mode with the loopback address; the page then mirrors
	/// that state. A failed setup saves nothing.
	async provisionWorker(onProvisioned: () => Promise<void>) {
		const current = await this.checkedSettings();
		if (!current) return;
		this.provisioning = true;
		try {
			this.localWorker = await api.profile.server.provisionLocalWorker(
				current,
				$state.snapshot(this.credentials)
			);
			const executor = this.form.remote.executor;
			if (this.localWorker.address) executor.workerAddress = this.localWorker.address;
			executor.mode = 'hostedWorker';
			// A reprovisioned worker keeps its journal. Adopt whatever
			// automation setting it actually runs.
			this.reconcileAutomation();
			// Provisioning saved the page's settings (in hosted-worker mode
			// with the loopback address), so the saved baseline moves to
			// what the page now shows.
			this.hasSavedSettings = true;
			this.clearSecrets();
			await this.refreshSavedCredentials();
			this.#commitBaseline();
			// Provisioning turned this profile into a hosted-worker remote.
			void serverSync.reconfigure();
			// The mounted panel owns its status cache. Reload it from the
			// installed worker before completing setup.
			await onProvisioned();
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
			this.form.remote.executor = { mode: 'local', workerAddress: '' };
			// The backend reverted the profile to Local sync. Patch the
			// saved baseline the same way so other unsaved edits stay dirty.
			const saved = this.#baseline;
			if (saved) {
				this.#baseline = {
					...saved,
					form: {
						...saved.form,
						remote: { ...saved.form.remote, executor: { mode: 'local', workerAddress: '' } }
					}
				};
			}
			void serverSync.reconfigure();
		} finally {
			this.workerBusy = false;
		}
	}

	/// Use the worker's confirmed setting, not the unsaved checkbox.
	pendingLabel(worker: WorkerStatus): string {
		if (!worker.autoDeployMods) return m.dedicatedServerDialog_localWorkerPendingManual();
		return worker.nextAttemptAt
			? m.dedicatedServerDialog_localWorkerPendingRetry()
			: m.dedicatedServerDialog_localWorkerPending();
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
			const { value, remember } = this.credentials.gamePassword;
			await api.profile.server.launch(current, value, remember);
			pushInfoToast({ message: m.toolBar_launchServer_started() });
		} finally {
			this.launching = false;
		}
	}
}
