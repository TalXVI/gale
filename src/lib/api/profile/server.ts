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
	ServerSyncPreview,
	ServerSyncResult,
	ServerSyncStatus,
	SyncConfigUpdatePolicy,
	WorkerStatus
} from '$lib/types';

export const getSettings = () =>
	invoke<ProfileServerSettings | null>('get_dedicated_server_settings');

/// Persists settings and credentials together. Empty credential fields
/// leave stored credentials untouched; `rememberCredentials = false`
/// clears them.
export const setSettings = (
	settings: ProfileServerSettings,
	remotePassword: string,
	workerToken: string,
	datHostPassword: string,
	rememberCredentials: boolean
) =>
	invoke('set_dedicated_server_settings', {
		request: { settings, remotePassword, workerToken, datHostPassword, rememberCredentials }
	});

/// `settings: null` launches with the profile's stored settings, so an
/// already-configured server starts immediately. `rememberPassword` controls
/// whether the provided (or stored) password stays in the credential store.
export const launch = (
	settings: ProfileServerSettings | null,
	password: string,
	rememberPassword: boolean
) =>
	invoke<DedicatedServerStatus>('launch_dedicated_server', {
		request: { settings, password, rememberPassword }
	});

/// Tests the remote transport. A successful test also persists the
/// settings and credentials like `setSettings` does.
export const testRemoteConnection = (
	settings: RemoteServerSettings,
	password: string,
	datHostPassword: string,
	rememberPassword: boolean
) =>
	invoke<RemoteConnectionTestResult>('test_remote_server_connection', {
		request: { settings, password, workerToken: '', datHostPassword, rememberPassword }
	});

/// Tests the worker's reachability, bearer token, and profile binding.
export const testWorkerConnection = (
	settings: RemoteServerSettings,
	workerToken: string,
	rememberPassword: boolean
) =>
	invoke<WorkerStatus>('test_worker_connection', {
		request: { settings, password: '', workerToken, datHostPassword: '', rememberPassword }
	});

export const getStatus = () => invoke<DedicatedServerStatus>('get_dedicated_server_status');

export const openDir = () => invoke('open_dedicated_server_dir');

export const forceStop = () => invoke('force_stop_dedicated_server');

// ---------- selective server synchronization ----------

export const getSyncStatus = (refresh: boolean, password = '', workerToken = '') =>
	invoke<ServerSyncStatus>('get_server_sync_status', {
		request: { refresh, password, workerToken }
	});

/// The restart policy is bound into the plan hash. A preview only stays
/// deployable while the selected policy is unchanged.
export const previewSync = (
	selection: DeploySelection,
	restartPolicy: RestartPolicy | null,
	password = '',
	workerToken = ''
) =>
	invoke<ServerSyncPreview>('preview_server_sync', {
		request: { selection, restartPolicy, password, workerToken }
	});

/// `force` takes over a *stale* foreign lease after the old executor is
/// confirmed stopped, the recovery path the preview's busy state shows.
/// Live leases always win.
export const deploySync = (
	selection: DeploySelection,
	planHash: string,
	restartPolicy: RestartPolicy | null,
	force: boolean,
	password = '',
	workerToken = ''
) =>
	invoke<ServerSyncResult>('deploy_server_sync', {
		request: { selection, planHash, restartPolicy, force, password, workerToken }
	});

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

/// Pushes the automation configuration to the bound worker and returns
/// the status it confirmed — the values the worker actually runs.
export const configureWorker = (
	autoSync: boolean,
	autoMods: boolean,
	restartPolicy: RestartPolicy,
	workerToken = ''
) =>
	invoke<WorkerStatus>('configure_worker', {
		request: { autoSync, autoMods, restartPolicy, workerToken }
	});

// ---------- managed local worker ("host worker on this PC") ----------

/// SCM state + status file + live API status for the managed worker.
export const getLocalWorkerStatus = () =>
	invoke<LocalWorkerStatus>('get_local_worker_status');

/// Provisions and installs the managed worker: a second Gale sign-in
/// gives the worker its own credentials, then one UAC-elevated step
/// registers and starts the Windows service.
export const provisionLocalWorker = (password = '', datHostPassword = '') =>
	invoke<LocalWorkerStatus>('provision_local_worker', {
		request: { password, datHostPassword }
	});

export const controlLocalWorker = (action: LocalWorkerAction) =>
	invoke<LocalWorkerStatus>('control_local_worker', { request: { action } });

/// Reinstalls the service with the bundled worker binary, keeping the
/// installed config, credentials, and journal.
export const updateLocalWorker = () =>
	invoke<LocalWorkerStatus>('update_local_worker');

/// Stops and removes the service and its state; the profile falls back
/// to Local sync when it still points at the managed worker.
export const uninstallLocalWorker = () =>
	invoke<LocalWorkerStatus>('uninstall_local_worker');
