import { mockIPC } from '@tauri-apps/api/mocks';
import { mount } from 'svelte';
import '../../src/app.css';

const workerMode = new URLSearchParams(location.search).get('mode') === 'worker';
const manyConfigs = new URLSearchParams(location.search).has('many');
let restartRequired = new URLSearchParams(location.search).has('restart');
const worker = { autoSync: false, autoMods: false, restartPolicy: 'manual' };
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

Object.assign(window, {
	calls,
	unexpected,
	hold: (cmd: string) => {
		held = cmd;
	},
	release: () => {
		held = '';
		release?.();
	}
});

mockIPC(async (cmd, args) => {
	calls.push({ cmd, args: structuredClone(args) });
	if (cmd === held)
		await new Promise<void>((resolve) => {
			release = resolve;
		});
	switch (cmd) {
		case 'get_game_info':
			return { active: null, all: [], favorites: [], lastUpdated: '' };
		case 'plugin:event|listen':
			return calls.length;
		case 'plugin:event|unlisten':
			return;
		case 'plugin:dialog|message':
			return 'Ok';
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
