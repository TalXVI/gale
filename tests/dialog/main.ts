import { mockIPC } from '@tauri-apps/api/mocks';
import { mount } from 'svelte';

const workerMode = new URLSearchParams(location.search).get('mode') === 'worker';
const worker = { autoSync: false, autoMods: false, restartPolicy: 'manual' };
const plan = {
	hash: 'approved-plan',
	uploads: [],
	removals: [],
	unmanaged: [],
	uploadBytes: 0,
	unchangedFiles: 0,
	modsPhase: true,
	configsPhase: true,
	conflicts: ['BepInEx/config/test.cfg'],
	configEntries: [
		{ path: 'BepInEx/config/test.cfg', action: 'pending', reason: 'deletedLocally', policy: 'ask' }
	]
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
		case 'get_server_sync_status':
			return {
				mode: workerMode ? 'worker' : 'local',
				worker: workerMode ? worker : null,
				warnings: []
			};
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
