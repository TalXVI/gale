import { invoke } from '$lib/invoke';
import type {
	DedicatedServerStatus,
	DeploySelection,
	LocalWorkerAction,
	LocalWorkerStatus,
	ProfileServerSettings,
	RemoteConnectionTestResult,
	RemoteServerSettings,
	RestartPolicy,
	SavedServerCredentials,
	SyncDialogPreferences,
	ServerSyncPreview,
	ServerSyncOperationProgress,
	ServerSyncResult,
	ServerSyncStatus,
	SyncConfigUpdatePolicy,
	WorkerStatus
} from '$lib/types';

export const getSettings = (options?: { quiet?: boolean }) =>
	invoke<ProfileServerSettings | null>('get_dedicated_server_settings', undefined, options);

export const getSyncDialogPreferences = () =>
	invoke<SyncDialogPreferences>('get_sync_dialog_preferences');

/// Which server credentials are stored for the active profile. Values
/// are never returned, only presence flags for "saved" markers.
export const getSavedCredentials = () =>
	invoke<SavedServerCredentials>('get_saved_server_credentials');

export const setSyncDialogPreferences = (preferences: SyncDialogPreferences) =>
	invoke('set_sync_dialog_preferences', { preferences });

/// One credential as the form keeps it: the typed value (may be empty)
/// and whether it should stay stored across saves.
export type CredentialInput = { value: string; remember: boolean };

export type ServerCredentials = {
	gamePassword: CredentialInput;
	remotePassword: CredentialInput;
	workerToken: CredentialInput;
	datHostPassword: CredentialInput;
};

const settingsRequest = (settings: ProfileServerSettings, credentials: ServerCredentials) => ({
	settings,
	remotePassword: credentials.remotePassword.value,
	rememberRemotePassword: credentials.remotePassword.remember,
	workerToken: credentials.workerToken.value,
	rememberWorkerToken: credentials.workerToken.remember,
	datHostPassword: credentials.datHostPassword.value,
	rememberDatHostPassword: credentials.datHostPassword.remember,
	gamePassword: credentials.gamePassword.value,
	rememberGamePassword: credentials.gamePassword.remember
});

/// Persists settings and credentials together. An empty value leaves a
/// stored credential untouched; each `remember` flag controls only its
/// own credential. Setting it to `false` clears that credential.
export const setSettings = (settings: ProfileServerSettings, credentials: ServerCredentials) =>
	invoke('set_dedicated_server_settings', { request: settingsRequest(settings, credentials) });

/// Saves `settings` and launches with them. `rememberPassword` controls
/// whether the provided (or stored) password stays in the credential store.
export const launch = (
	settings: ProfileServerSettings,
	password: string,
	rememberPassword: boolean
) =>
	invoke<DedicatedServerStatus>('launch_dedicated_server', {
		request: { settings, password, rememberPassword }
	});

/// Tests the remote transport without saving settings or credentials.
export const testRemoteConnection = (settings: RemoteServerSettings, password: string) =>
	invoke<RemoteConnectionTestResult>('test_remote_server_connection', {
		request: { settings, password, workerToken: '' }
	});

/// Tests the worker's reachability, bearer token, and profile binding.
export const testWorkerConnection = (settings: RemoteServerSettings, workerToken: string) =>
	invoke<WorkerStatus>('test_worker_connection', {
		request: { settings, password: '', workerToken }
	});

export const getStatus = () => invoke<DedicatedServerStatus>('get_dedicated_server_status');

export const openDir = () => invoke('open_dedicated_server_dir');

export const forceStop = () => invoke('force_stop_dedicated_server');

// ---------- selective server synchronization ----------

export const getSyncStatus = (
	refresh: boolean,
	password = '',
	workerToken = '',
	options?: { quiet?: boolean }
) =>
	invoke<ServerSyncStatus>(
		'get_server_sync_status',
		{
			request: { refresh, password, workerToken }
		},
		options
	);

export const getSyncProgress = (workerToken = '') =>
	invoke<ServerSyncOperationProgress | null>('get_server_sync_progress', {
		request: { workerToken }
	});

/// Typed credentials that override the stored ones for one request.
export type CredentialOverrides = { password: string; workerToken: string };

/// The restart policy is bound into the plan hash. A preview only stays
/// deployable while the selected policy is unchanged.
export type PreviewSyncRequest = CredentialOverrides & {
	selection: DeploySelection;
	restartPolicy: RestartPolicy | null;
	runId: string;
};

export const previewSync = (request: PreviewSyncRequest) =>
	invoke<ServerSyncPreview>('preview_server_sync', { request });

/// `force` takes over a *stale* foreign lease after the old executor is
/// confirmed stopped, the recovery path the preview's busy state shows.
/// Live leases always win.
export type DeploySyncRequest = PreviewSyncRequest & { planHash: string; force: boolean };

export const deploySync = (request: DeploySyncRequest) =>
	invoke<ServerSyncResult>('deploy_server_sync', { request });

/// The backend derives the publication pin itself, so callers never
/// supply it and a policy can't be anchored to the wrong revision.
export const setConfigPolicy = (
	path: string,
	policy: SyncConfigUpdatePolicy,
	password = '',
	workerToken = ''
) =>
	invoke('set_server_config_policy', {
		request: { path, policy, password, workerToken }
	});

export const acknowledgeExternalRestart = (password = '', workerToken = '') =>
	invoke('acknowledge_external_server_restart', {
		request: { password, workerToken }
	});

/// Pushes the automation configuration to the bound worker and returns
/// the status it confirmed. These are the values the worker actually runs.
export const configureWorker = (
	autoDeployMods: boolean,
	restartPolicy: RestartPolicy,
	workerToken = ''
) =>
	invoke<WorkerStatus>('configure_worker', {
		request: { autoDeployMods, restartPolicy, workerToken }
	});

// ---------- managed local worker ("host worker on this PC") ----------

/// SCM state + status file + live API status for the managed worker.
export const getLocalWorkerStatus = (options?: { quiet?: boolean }) =>
	invoke<LocalWorkerStatus>('get_local_worker_status', undefined, options);

/// Provisions and installs the managed worker: a second Gale sign-in
/// gives the worker its own credentials, then one UAC-elevated step
/// registers and starts the Windows service. The worker is set up from
/// `settings`, which are saved, with the credentials, only once the
/// service runs; a failed setup leaves the saved settings untouched.
export const provisionLocalWorker = (
	settings: ProfileServerSettings,
	credentials: ServerCredentials
) =>
	invoke<LocalWorkerStatus>('provision_local_worker', {
		request: settingsRequest(settings, credentials)
	});

export const controlLocalWorker = (action: LocalWorkerAction) =>
	invoke<LocalWorkerStatus>('control_local_worker', { request: { action } });

/// Reinstalls the service with the bundled worker binary, keeping the
/// installed config, credentials, and journal.
export const updateLocalWorker = () => invoke<LocalWorkerStatus>('update_local_worker');

/// Stops and removes the service and its state; the profile falls back
/// to Local sync when it still points at the managed worker.
export const uninstallLocalWorker = () => invoke<LocalWorkerStatus>('uninstall_local_worker');
