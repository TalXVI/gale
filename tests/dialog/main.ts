import { mockIPC } from '@tauri-apps/api/mocks';
import { mount } from 'svelte';
import '../../src/app.css';

let settings: unknown = null;
let serverRunning = new URLSearchParams(location.search).has('running');
const cancelStop = new URLSearchParams(location.search).has('cancelStop');
const workerMode = new URLSearchParams(location.search).get('mode') === 'worker';
const manyConfigs = new URLSearchParams(location.search).has('many');
let restartRequired = new URLSearchParams(location.search).has('restart');
const worker = {
	autoSync: false,
	autoMods: false,
	restartPolicy: 'manual',
	lastError: null as string | null,
	pollError: null as string | null
};
const profileId = new URLSearchParams(location.search).get('profile') ?? 'first';
const preferences = JSON.parse(
	sessionStorage.getItem('mock-profile-preferences') ?? '{}'
) as Record<string, { scope: string; restartPolicy: string }>;
const configEntries = manyConfigs
	? Array.from({ length: 133 }, (_, index) => ({
			path: `BepInEx/config/file-${String(index).padStart(3, '0')}.cfg`,
			action: index % 4 === 3 && index < 124 ? 'pending' : 'markApplied',
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
const plan = {
	hash: 'approved-plan',
	uploads: [],
	removals: [],
	unmanaged: [],
	uploadBytes: 0,
	unchangedFiles: 0,
	modsPhase: true,
	configsPhase: true,
	conflicts: configEntries.filter((entry) => entry.action === 'pending').map((entry) => entry.path),
	configEntries
};

const calls: { cmd: string; args: any }[] = [];
const unexpected: string[] = [];
let held = '';
let release: (() => void) | undefined;
const failing = new Set<string>();
let workerProgress: Record<string, unknown> | null = null;
let progressPolls = 0;
const progressListeners = new Set<number>();

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
		totalPhases: 16,
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
	unexpected,
	hold: (cmd: string) => {
		held = cmd;
	},
	release: () => {
		held = '';
		release?.();
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
	if (cmd === 'get_server_sync_progress') {
		progressPolls++;
		return workerProgress ? progressPayload(workerProgress) : null;
	}
	calls.push({ cmd, args: structuredClone(args) });
	if (cmd === held)
		await new Promise<void>((resolve) => {
			release = resolve;
		});
	if (failing.has(cmd))
		throw { message: `Simulated failure: ${cmd}`, detail: `Simulated failure: ${cmd}` };
	switch (cmd) {
		case 'get_dedicated_server_settings':
			return settings;
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
			return { supported: true, service: 'notInstalled', ownership: 'none', warnings: [] };
		case 'set_dedicated_server_settings':
			settings = structuredClone((args as any).request.settings);
			return;
		case 'test_remote_server_connection':
			return { status: 'connected', encrypted: true };
		case 'test_worker_connection':
			return { workerId: 'test-worker', autoSync: false };
		case 'get_game_info':
			return { active: null, all: [], favorites: [], lastUpdated: '' };
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
		case 'log_err':
			return;
		case 'get_server_sync_status':
			return {
				mode: workerMode ? 'worker' : 'local',
				worker: workerMode ? worker : null,
				server: restartRequired
					? {
							restartRequired,
							pendingConfigs: 0,
							modsRevision: null,
							lastOperation: null,
							lease: null
						}
					: null,
				warnings: []
			};
		case 'get_sync_dialog_preferences':
			return preferences[profileId] ?? { scope: 'both', restartPolicy: 'manual' };
		case 'set_sync_dialog_preferences':
			preferences[profileId] = (args as any).preferences;
			sessionStorage.setItem('mock-profile-preferences', JSON.stringify(preferences));
			return;
		case 'acknowledge_external_server_restart':
			restartRequired = false;
			return;
		case 'preview_server_sync':
			return { plan, warnings: [] };
		case 'set_server_config_policy':
			// The real command returns nothing; the dialog reflects the saved
			// policy itself rather than waiting on a mutated preview.
			return;
		case 'configure_worker':
			Object.assign(worker, (args as any).request);
			return worker;
		case 'deploy_server_sync':
			return {
				plan,
				state: {},
				summary: { uploadedFiles: 0, uploadedBytes: 0, removedFiles: 0, unchangedFiles: 0 },
				restart: 'notRequired',
				failedConfigWrites: [],
				warnings: []
			};
		default:
			unexpected.push(cmd);
			throw new Error(`Unexpected IPC: ${cmd}`);
	}
});

// Install IPC before importing the application's event subscriptions.
const { default: Harness } = await import('./Harness.svelte');
mount(Harness, { target: document.getElementById('app')! });
