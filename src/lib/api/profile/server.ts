import { invoke } from '$lib/invoke';
import type {
	DedicatedServerStatus,
	DeploySelection,
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

export const previewSync = (selection: DeploySelection, password = '', workerToken = '') =>
	invoke<ServerSyncPreview>('preview_server_sync', {
		request: { selection, password, workerToken }
	});

export const deploySync = (
	selection: DeploySelection,
	planHash: string,
	restartPolicy: RestartPolicy | null,
	password = '',
	workerToken = ''
) =>
	invoke<ServerSyncResult>('deploy_server_sync', {
		request: { selection, planHash, restartPolicy, password, workerToken }
	});

export const setConfigPolicy = (
	path: string,
	policy: SyncConfigUpdatePolicy,
	pinnedAt: string | null,
	password = '',
	workerToken = ''
) =>
	invoke('set_server_config_policy', {
		request: { path, policy, pinnedAt, password, workerToken }
	});

export const configureWorker = (
	autoSync: boolean,
	autoMods: boolean,
	restartPolicy: RestartPolicy,
	workerToken = ''
) =>
	invoke('configure_worker', {
		request: { autoSync, autoMods, restartPolicy, workerToken }
	});
