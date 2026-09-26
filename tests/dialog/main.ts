import { mockIPC } from '@tauri-apps/api/mocks';
import { mount } from 'svelte';
import '../../src/app.css';

const params = new URLSearchParams(location.search);
const saved = params.has('saved');
const workerMode = params.get('mode') === 'worker';
const statusCase = params.get('status') ?? '';
let settings: unknown = params.has('unset')
	? null
	: {
			location: params.has('local') ? 'local' : 'remote',
			serverName: 'Test server',
			world: 'Dedicated',
			port: 2456,
			publicServer: true,
			crossplay: false,
			extraArgs: '',
			remote: {
				protocol: 'sftp',
				host: 'example.test',
				port: 22,
				username: 'test-user',
				serverDirectory: '/srv/server',
				authentication: 'password',
				privateKeyPath: '',
				trustedHostKey: null,
				trustedCertificate: null,
				syncMode: workerMode ? 'worker' : 'local',
				worker: {
					address: workerMode ? 'https://worker.example.test' : '',
					hosted: false,
					autoDeployMods: false
				},
				hostControl: { provider: 'none', datHostServerId: '', datHostUsername: '' },
				restartPolicy: 'manual'
			}
		};
let serverRunning = params.has('running');
const cancelStop = params.has('cancelStop');
const manyConfigs = params.has('many');
const plannedUnchanged = Number(params.get('unchanged') ?? '0');
const plannedUnmanaged = Number(params.get('unmanaged') ?? '0');
let restartRequired = params.has('restart');
const worker = {
	workerId: 'test-worker',
	profileId: 'sync-1',
	autoDeployMods: false,
	restartPolicy: 'manual',
	observedRevision: null as string | null,
	lastError: null as string | null,
	pollError: null as string | null,
	pendingRevision: null as string | null,
	nextAttemptAt: null as string | null,
	lastDeployedRevision: null as string | null,
	busy: null,
	lastOperation: null,
	server: null
};
if (params.has('deployed')) worker.lastDeployedRevision = '2026-09-21T00:00:00Z';
if (params.has('wpending')) worker.pendingRevision = '2026-09-23T12:00:00Z';
let workerRefreshFails = false;
const profileId = params.get('profile') ?? 'first';
let activeId = 1;
const preferences = JSON.parse(
	sessionStorage.getItem('mock-profile-preferences') ?? '{}'
) as Record<string, { restartPolicy: string }>;
const configEntries = manyConfigs
	? Array.from({ length: 12 }, (_, index) => ({
			path: `BepInEx/config/file-${String(index).padStart(3, '0')}.cfg`,
			action: index % 4 === 3 ? 'pending' : 'markApplied',
			reason: 'modifiedLocally',
			policy: 'ask'
		}))
	: [
			{
				path: 'BepInEx/config/test.cfg',
				action: 'pending',
				reason: 'deletedLocally',
				policy: 'ask'
			}
		];
// The plan mirrors the requested selection: config entries only exist
// when the preview targeted configs — a mods sync never computes them.
function planFor(selection: { includeMods: boolean; includeConfigs: boolean }) {
	return {
		hash: 'approved-plan',
		uploads: [],
		removals: [],
		unmanaged: Array.from({ length: plannedUnmanaged }, (_, index) => `Extra/file-${index}.dat`),
		uploadBytes: 0,
		unchangedFiles: selection.includeMods ? plannedUnchanged : 0,
		modsPhase: selection.includeMods,
		configsPhase: selection.includeConfigs,
		requiresRestart: false,
		conflicts: selection.includeConfigs
			? configEntries
					.filter((entry) => entry.action === 'pending')
					.map((entry) => ({ path: entry.path, reason: entry.reason }))
			: [],
		configEntries: selection.includeConfigs ? configEntries : []
	};
}

// Local-mode server states the status panel reports in plain language.
function serverState() {
	switch (statusCase) {
		case 'upToDate':
			return {
				modsRevision: '2026-09-22T00:00:00Z',
				restartRequired,
				lastOperation: {
					id: 'op-1',
					executor: 'local',
					kind: 'manual',
					workerId: null,
					publicationRevision: '2026-09-22T00:00:00Z',
					modsRevision: '2026-09-22T00:00:00Z',
					status: 'succeeded',
					summary: {
						uploadedFiles: 3,
						uploadedBytes: 3072,
						removedFiles: 0,
						configWrites: 0,
						unchangedFiles: 160
					},
					restart: 'notRequired',
					error: null,
					startedAt: '2026-09-22T00:00:00Z',
					finishedAt: '2026-09-22T00:01:00Z'
				},
				lease: null
			};
		default:
			return restartRequired
				? { restartRequired, modsRevision: null, lastOperation: null, lease: null }
				: null;
	}
}

const calls: { cmd: string; args: any }[] = [];
let held = '';
const heldResolvers = new Set<() => void>();
const heldInitialSettings = new Set<() => void>();
const failing = new Set<string>();
if (params.has('failStatus')) failing.add('get_server_sync_status');
if (params.has('holdStatus')) held = 'get_server_sync_status';
let workerProgress: Record<string, unknown> | null = null;
let progressPolls = 0;
const progressListeners = new Set<number>();
let localWorker: Record<string, unknown> = {
	supported: true,
	service: 'notInstalled',
	binding: null,
	ownership: 'none',
	run: null,
	worker: null,
	workerError: null,
	stoppedForShutdown: false,
	updateAvailable: false,
	warnings: []
};

function latestRun() {
	return calls.findLast((call) => ['preview_server_sync', 'deploy_server_sync'].includes(call.cmd));
}

function progressPayload(patch: Record<string, unknown>) {
	const operation = latestRun();
	return {
		runId: operation?.args.request.runId ?? '',
		operation: operation?.cmd === 'deploy_server_sync' ? 'deploy' : 'preview',
		status: 'running',
		phase: 'verifyingPayload',
		completedPhases: 7,
		totalPhases: operation?.cmd === 'deploy_server_sync' ? 14 : 10,
		completed: 0,
		total: 24,
		completedBytes: null,
		totalBytes: null,
		item: null,
		...patch
	};
}

Object.assign(window, {
	calls,
	hold: (cmd: string) => {
		held = cmd;
	},
	release: () => {
		held = '';
		for (const resolve of heldResolvers) resolve();
		heldResolvers.clear();
	},
	releaseInitialSettings: () => {
		for (const resolve of heldInitialSettings) resolve();
		heldInitialSettings.clear();
	},
	fail: (cmd: string) => {
		failing.add(cmd);
	},
	unfail: (cmd: string) => {
		failing.delete(cmd);
	},
	setWorkerProgress: (patch: Record<string, unknown> | null) => {
		workerProgress = patch;
	},
	setWorkerErrors: (pollError: string | null, lastError: string | null) => {
		worker.pollError = pollError;
		worker.lastError = lastError;
	},
	setWorkerPending: (pendingRevision: string | null) => {
		worker.pendingRevision = pendingRevision;
	},
	failWorkerRefresh: (on: boolean) => {
		workerRefreshFails = on;
	},
	progressPolls: () => progressPolls,
	emitProgress: (patch: Record<string, unknown>) => {
		for (const handler of progressListeners) {
			(window as any).__TAURI_INTERNALS__.runCallback(handler, {
				event: 'server_sync_operation_progress',
				payload: progressPayload(patch)
			});
		}
	}
});

mockIPC(async (cmd, args) => {
	// The profile a call belongs to is fixed when it arrives — a held
	// response still describes the profile it was issued for.
	const callProfile = activeId;
	if (cmd === 'get_server_sync_progress') {
		progressPolls++;
		return workerProgress ? progressPayload(workerProgress) : null;
	}
	calls.push({ cmd, args: structuredClone(args) });
	if (cmd === held)
		await new Promise<void>((resolve) => {
			heldResolvers.add(resolve);
		});
	if (
		cmd === 'get_dedicated_server_settings' &&
		callProfile === 1 &&
		params.has('holdInitialSettings')
	)
		await new Promise<void>((resolve) => {
			heldInitialSettings.add(resolve);
		});
	if (failing.has(cmd))
		throw { message: `Simulated failure: ${cmd}`, detail: `Simulated failure: ${cmd}` };
	switch (cmd) {
		case 'get_game_info':
			return {
				active: {
					name: 'Valheim',
					slug: 'valheim',
					platforms: ['steam'],
					favorite: false,
					modLoader: 'BepInEx',
					popular: false,
					backends: ['Thunderstore'],
					dedicatedServer: { platforms: ['steam'], defaultPort: 2456 }
				},
				all: [],
				favorites: [],
				lastUpdated: ''
			};
		case 'get_profile_info':
			return {
				profiles: [
					{
						id: 1,
						name: 'Test profile',
						modCount: 3,
						sync: {
							id: 'sync-1',
							owner: { discordId: '1', name: 'owner', displayName: 'Owner', avatar: null },
							syncedAt: '2026-01-01T00:00:00Z',
							updatedAt: '2026-09-22T00:00:00Z',
							missing: false
						},
						customArgs: '',
						missing: false
					},
					{
						id: 2,
						name: 'Second profile',
						modCount: 0,
						sync: null,
						customArgs: '',
						missing: false
					}
				],
				activeId
			};
		case 'get_categories':
			return [];
		case 'get_user':
			return null;
		case 'get_dedicated_server_settings':
			if (settings == null) return null;
			// Profile 2 is a plain manual-sync profile — the navbar must
			// never poll or badge it as a worker remote. `?worker2=1`
			// makes it a second worker-mode profile instead.
			if (callProfile === 2) {
				const s = settings as any;
				return {
					...s,
					remote: {
						...s.remote,
						syncMode: params.has('worker2') ? 'worker' : 'local',
						worker: {
							...s.remote.worker,
							address: params.has('worker2') ? 'https://worker2.example.test' : ''
						}
					}
				};
			}
			return settings;
		case 'get_saved_server_credentials':
			return {
				gamePassword: saved,
				sftpPassword: saved,
				ftpPassword: saved,
				sshKeyPassphrase: saved,
				datHostPassword: saved,
				workerToken: saved
			};
		case 'get_dedicated_server_status':
			return serverRunning
				? {
						state: 'running',
						profileId: 1,
						gameSlug: 'valheim',
						pid: 123,
						serverDir: '/test',
						stopping: false
					}
				: { state: 'stopped' };
		case 'force_stop_dedicated_server':
			serverRunning = false;
			return;
		case 'get_local_worker_status':
			return localWorker;
		case 'provision_local_worker':
			localWorker = {
				...localWorker,
				service: 'running',
				ownership: 'owned',
				binding: {
					workerId: 'local-worker',
					profileId: 'sync-1',
					listen: '127.0.0.1:8472',
					address: 'http://127.0.0.1:8472'
				},
				run: {
					workerId: 'local-worker',
					profileId: 'sync-1',
					pid: 456,
					phase: 'running',
					at: '2026-09-25T00:00:00Z'
				},
				worker: { ...worker, workerId: 'local-worker' }
			};
			return localWorker;
		case 'set_dedicated_server_settings':
			settings = structuredClone((args as any).request.settings);
			return;
		case 'test_remote_server_connection':
			return { status: 'connected', encrypted: true };
		case 'test_worker_connection':
			return { workerId: 'test-worker', autoDeployMods: false };
		case 'plugin:event|listen':
			if ((args as any).event === 'server_sync_operation_progress') {
				progressListeners.add((args as any).handler);
				return (args as any).handler;
			}
			return calls.length;
		case 'plugin:event|unlisten':
			progressListeners.delete((args as any).id);
			return;
		case 'plugin:dialog|message':
			return cancelStop ? 'Cancel' : 'Ok';
		case 'plugin:store|load':
			return;
		case 'plugin:store|entries':
			return [];
		case 'plugin:store|set':
		case 'plugin:store|save':
			return;
		case 'log_err':
			return;
		case 'get_server_sync_status':
			// The second profile has never been published or deployed —
			// a profile switch must show its state, not profile 1's.
			// `?worker2=1` instead makes it a worker remote with pending
			// work, so its badge shows amber.
			if (callProfile === 2)
				return params.has('worker2')
					? {
							mode: 'worker',
							worker: { ...worker, pendingRevision: '2026-09-23T12:00:00Z' },
							publicationRevision: '2026-09-25T00:00:00Z',
							server: null,
							credentialRequired: false,
							warnings: []
						}
					: {
							mode: 'local',
							worker: null,
							publicationRevision: null,
							server: null,
							credentialRequired: false,
							warnings: []
						};
			const syncStatus = {
				mode: workerMode ? 'worker' : 'local',
				worker: workerMode ? worker : null,
				// A publication normally exists — `nopub` models a profile
				// that has never been published.
				publicationRevision: params.has('nopub') ? null : '2026-09-22T00:00:00Z',
				server: serverState(),
				credentialRequired: false,
				warnings: [] as string[]
			};
			// Mirrors the backend: a failed live refresh on the worker
			// returns an Ok status without the worker payload plus a
			// warning, rather than throwing.
			if (
				workerRefreshFails &&
				(args as any).request.refresh === true &&
				syncStatus.mode === 'worker'
			) {
				return {
					...syncStatus,
					worker: null,
					server: null,
					warnings: ['could not read server status: connection refused']
				};
			}
			return syncStatus;
		case 'get_sync_dialog_preferences':
			return preferences[profileId] ?? { restartPolicy: 'manual' };
		case 'set_sync_dialog_preferences':
			preferences[profileId] = (args as any).preferences;
			sessionStorage.setItem('mock-profile-preferences', JSON.stringify(preferences));
			return;
		case 'acknowledge_external_server_restart':
			restartRequired = false;
			return;
		case 'preview_server_sync':
			return { plan: planFor((args as any).request.selection), warnings: [] };
		case 'set_server_config_policy':
			// The real command returns nothing; the page reflects the saved
			// policy itself rather than waiting on a mutated preview.
			return;
		case 'deploy_server_sync':
			return {
				plan: planFor((args as any).request.selection),
				state: {},
				summary: {
					uploadedFiles: 0,
					uploadedBytes: 0,
					removedFiles: 0,
					configWrites: 0,
					unchangedFiles: 0
				},
				restart: 'notRequired',
				failedConfigWrites: [],
				warnings: []
			};
		default:
			throw new Error(`Unexpected IPC: ${cmd}`);
	}
});

document.documentElement.classList.add('dark');

// Install IPC before importing the application's event subscriptions.
const { default: Harness } = await import('./Harness.svelte');
const { default: profiles } = await import('$lib/state/profile.svelte');
Object.assign(window, {
	switchProfile: async (id: number) => {
		activeId = id;
		await profiles.refresh();
	}
});
mount(Harness, { target: document.getElementById('app')! });
