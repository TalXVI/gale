import { mockConvertFileSrc, mockIPC } from '@tauri-apps/api/mocks';
import { mount } from 'svelte';
import type { Mod, ProfileMod } from '$lib/types';
import { Backend, ModType } from '$lib/types';
import Toasts from '$lib/components/misc/Toasts.svelte';
import '../../src/app.css';

const params = new URLSearchParams(location.search);
const calls: { cmd: string; args: unknown }[] = [];
Object.assign(window, { calls, navigations: [] });
let livePulled = false;

const local: ProfileMod = {
	enabled: false,
	configFile: 'LocalMod.cfg',
	alternateBackend: null,
	data: {
		name: 'LocalMod',
		description: 'Disabled local mod',
		version: '1.0.0',
		uuid: '00000000-0000-0000-0000-000000000002',
		versionUuid: '00000000-0000-0000-0000-000000000002',
		icon: 'C:/test/LocalMod/icon.png',
		type: ModType.Local,
		backend: Backend.Thunderstore,
		versions: [],
		dependencies: null,
		suggestions: null,
		categories: null,
		author: null,
		rating: null,
		downloads: null,
		fileSize: 100,
		websiteUrl: null,
		donateUrl: null,
		isPinned: false,
		isDeprecated: false,
		containsNsfw: false,
		lastUpdated: null
	}
};

function remote(backend: Backend, version: string, versionUuid: string): Mod {
	return {
		...local.data,
		name: 'DualSourceMod',
		author: 'Author',
		type: ModType.Remote,
		icon: null,
		backend,
		version,
		versionUuid,
		versions: [{ name: version, uuid: versionUuid }],
		isDeprecated: params.get('deprecated') === backend
	};
}

mockIPC((cmd, args) => {
	calls.push({ cmd, args });
	if (cmd.startsWith('plugin:event|')) return 1;
	if (cmd.startsWith('plugin:store|')) {
		if (cmd.endsWith('load')) return 1;
		if (cmd.endsWith('get')) return [null, false];
		if (cmd.endsWith('entries')) return [];
		return;
	}
	switch (cmd) {
		case 'get_game_info': {
			const game = {
				slug: 'valheim',
				name: 'Valheim',
				backends: [Backend.Thunderstore, Backend.Hexium],
				modLoader: 'BepInEx'
			};
			return { active: game, all: [game], favorites: [], lastUpdated: '' };
		}
		case 'get_profile_info':
			return { activeId: 1, profiles: [{ id: 1, name: 'Review', sync: null }] };
		case 'query_profile':
			if (params.has('live')) {
				const data = remote(Backend.Thunderstore, '1.1.3', '00000000-0000-0000-0000-000000000013');
				data.versions = livePulled
					? [{ name: '1.1.4', uuid: '00000000-0000-0000-0000-000000000014' }, ...data.versions]
					: data.versions;
				return {
					mods: [{ enabled: true, configFile: null, alternateBackend: null, data }],
					unknownMods: [],
					totalModCount: 1,
					updates: livePulled
						? [
								{
									fullName: 'Author-DualSourceMod',
									ignore: false,
									isCrossBackend: false,
									updatedId: {
										packageUuid: data.uuid,
										versionUuid: '00000000-0000-0000-0000-000000000014',
										backend: Backend.Thunderstore
									},
									old: '1.1.3',
									new: '1.1.4'
								}
							]
						: []
				};
			}
			return {
				mods: [local],
				unknownMods: [1, 3].map((n) => ({
					uuid: `00000000-0000-0000-0000-${String(n).padStart(12, '0')}`,
					fullName: `Author-Missing${n}-1.0.0`,
					backend: Backend.Thunderstore
				})),
				totalModCount: 3,
				updates: []
			};
		case 'query_thunderstore':
			return [
				{
					isInstalled: false,
					data: {
						thunderstore: remote(
							Backend.Thunderstore,
							livePulled ? '1.1.4' : (params.get('ts') ?? '1.0.0'),
							'00000000-0000-0000-0000-000000000010'
						),
						hexium: remote(
							Backend.Hexium,
							params.get('hx') ?? '1.0.1',
							'00000000-0000-0000-0000-000000000011'
						)
					}
				}
			];
		case 'get_dedicated_server_status':
			return params.has('locked')
				? {
						state: 'running',
						profileId: 1,
						gameSlug: 'valheim',
						serverDir: 'server',
						stopping: false
					}
				: { state: 'stopped' };
		case 'get_categories':
			return [];
		case 'get_config_files':
			return [{ name: 'LocalMod', relativePath: 'LocalMod.cfg', type: 'ok', sections: [] }];
		case 'get_user':
		case 'get_local_markdown':
		case 'get_markdown':
			return null;
		case 'get_prefs':
			return { backendSkipConfirm: true };
		case 'get_download_size':
			return 0;
		case 'pull_live_mod':
			return new Promise<string>((resolve, reject) => {
				setTimeout(
					() => {
						if (params.has('liveError')) {
							reject({
								message: 'Required dependency Author-Loader-2.0.0 is not available in the catalog',
								detail: 'Dependency not available'
							});
						} else {
							livePulled = true;
							resolve('1.1.4');
						}
					},
					params.has('liveDelay') ? 1000 : 0
				);
			});
		case 'is_installing':
			return false;
		case 'force_remove_mods':
		case 'install_mod':
		case 'stop_querying_thunderstore':
		case 'log_err':
			return;
		default:
			throw new Error(`Unexpected mod-test IPC: ${cmd}`);
	}
});
mockConvertFileSrc('windows');
mount(Toasts, { target: document.body });

if (params.has('browse')) {
	const { default: Page } = await import('../../src/routes/browse/+page.svelte');
	mount(Page, { target: document.getElementById('app')! });
} else {
	const { default: Page } = await import('../../src/routes/+page.svelte');
	mount(Page, { target: document.getElementById('app')! });
}
