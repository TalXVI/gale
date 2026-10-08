import { invoke } from '$lib/invoke';
import {
	type Backend,
	type MarkdownType,
	type ModId,
	type BrowsedMod,
	type PackageCategory,
	type QueryModsArgs
} from '$lib/types';

export const query = (args: QueryModsArgs) => invoke<BrowsedMod[]>('query_thunderstore', { args });
export const stopQuerying = () => invoke('stop_querying_thunderstore');
export const triggerModFetch = () => invoke('trigger_mod_fetch');
const pendingLivePulls = new Map<string, Promise<string>>();

export async function pullLive(packageUuid: string, game: string): Promise<string> {
	const key = `${game}:${packageUuid}`;
	const pending = pendingLivePulls.get(key);
	if (pending) return pending;

	const request = invoke<string>('pull_live_mod', { packageUuid, game });
	pendingLivePulls.set(key, request);
	try {
		return await request;
	} finally {
		pendingLivePulls.delete(key);
	}
}

export const getMarkdown = (id: ModId, type: MarkdownType) =>
	invoke<string | null>('get_markdown', { modRef: id, kind: type });
export const setToken = (backend: Backend, token: string) =>
	invoke('set_api_token', { backend, token });
export const hasToken = (backend: Backend) => invoke<boolean>('has_api_token', { backend });
export const clearToken = (backend: Backend) => invoke('clear_api_token', { backend });
export const getCategories = (gameSlug: string) =>
	invoke<PackageCategory[]>('get_categories', { game: gameSlug });
