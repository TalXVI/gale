import { writable } from 'svelte/store';
import { Backend, ModType, type ModContextItem } from './types';
import { open } from '@tauri-apps/plugin-shell';
import { m } from './paraglide/messages';
import { communityUrl } from './util';
import { writeText } from '@tauri-apps/plugin-clipboard-manager';
import { pushInfoToast } from './toast';
import * as thunderstore from './api/thunderstore';
import games from './state/game.svelte';

export function pullLiveContextItem(refresh: () => Promise<void>): ModContextItem {
	return {
		label: m.page_modContextItem_pullLive(),
		icon: 'mdi:cloud-download-outline',
		showFor: (mod) => mod.type === ModType.Remote && mod.backend === Backend.Thunderstore,
		onclick: async (mod) => {
			const game = games.active?.slug;
			if (!game) return;
			pushInfoToast({ message: m.page_modContextItem_pullLive_loading({ name: mod.name }) });
			try {
				const version = await thunderstore.pullLive(mod.uuid, game);
				await refresh();
				pushInfoToast({
					message: m.page_modContextItem_pullLive_message({ name: mod.name, version })
				});
			} catch {
				// invoke reports failures through the existing error toast.
			}
		}
	};
}

function openIfNotNull(url: string | null) {
	if (url !== null) open(url);
}

export const defaultContextItems: ModContextItem[] = [
	{
		label: m.page_modContextItem_openWebsite(),
		icon: 'mdi:open-in-new',
		onclick: (mod) => openIfNotNull(mod.websiteUrl),
		showFor: (mod) => mod.websiteUrl !== null && mod.websiteUrl.length > 0
	},
	{
		label: m.page_modContextItem_copyLink(),
		icon: 'mdi:link-variant',
		onclick: async (mod) => {
			const url = communityUrl(mod.backend, mod.author ?? '', mod.name);
			await writeText(url);
			pushInfoToast({
				message: m.page_modContextItem_copyLink_message()
			});
		},
		showFor: (mod) => mod.type === ModType.Remote
	},
	{
		label: m.page_modContextItem_donate(),
		icon: 'mdi:heart',
		onclick: (mod) => openIfNotNull(mod.donateUrl),
		showFor: (mod) => mod.donateUrl !== null
	}
];

export let activeContextMenu = writable<string | null>(null);
