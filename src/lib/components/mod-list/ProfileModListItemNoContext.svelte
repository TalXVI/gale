<script lang="ts">
	import type { AvailableUpdate, Mod, ProfileMod } from '../../types';
	import type { MouseEventHandler } from 'svelte/elements';
	import { formatModName, isNonReleaseVersion, modIconSrc } from '$lib/util';
	import Icon from '@iconify/svelte';
	import Tooltip from '../ui/Tooltip.svelte';
	import { m } from '$lib/paraglide/messages';
	import type { Snippet } from 'svelte';
	import type { ClassValue } from 'clsx';

	type Props = {
		mod: ProfileMod;
		index?: number;
		class?: ClassValue;
		selected?: boolean;
		onclick?: MouseEventHandler<HTMLDivElement>;
		leading?: Snippet;
		trailing?: Snippet;
		update?: AvailableUpdate;
	};

	let {
		mod,
		index = 0,
		class: classProp,
		selected,
		onclick,
		leading,
		trailing,
		update
	}: Props = $props();
</script>

<!-- svelte-ignore a11y_click_events_have_key_events -->
<div
	{onclick}
	role="button"
	tabindex="-1"
	class={[
		classProp,
		!mod.enabled && 'opacity-70',
		// can't use tailwind's odd: because the items are wrapped the virtual list item elements
		selected
			? 'dark:bg-primary-700 bg-primary-200'
			: index % 2 === 1 && 'dark:bg-primary-900/30 bg-primary-50',
		selected
			? 'dark:border-primary-500 border-primary-300'
			: 'dark:hover:bg-primary-700 hover:bg-primary-200 border-transparent',
		'group text-primary-600 dark:text-primary-300 mx-2 grid grid-cols-[auto_3fr_1fr_auto] items-center rounded-lg border p-2 lg:grid-cols-[auto_3fr_1fr_1fr_auto]'
	]}
>
	{#if leading}
		{@render leading()}
	{:else}
		<div></div>
	{/if}

	<div class="flex items-center overflow-hidden">
		<img
			src={modIconSrc(mod.data, mod.enabled)}
			alt={mod.data.name}
			class="mr-3 size-12 rounded-md"
		/>

		<div class="mr-2 shrink overflow-hidden">
			<div
				class={[
					mod.enabled ? 'text-primary-800' : 'line-through',
					'font-medium',
					'flex items-center',
					'dark:text-white'
				]}
			>
				<span class="mr-2 truncate">{formatModName(mod.data.name)}</span>

				{#if mod.data.isPinned}
					<Icon class="text-primary-500 dark:text-primary-400 mr-1 shrink-0" icon="mdi:pin" />
				{/if}
				{#if mod.data.isDeprecated}
					{#if mod.alternateBackend?.reason === 'olderButNotDeprecated'}
						<Tooltip
							text={m.page_modDetails_olderNotDeprecated({
								site: mod.alternateBackend.backend,
								version: mod.alternateBackend.latestVersion
							})}
						>
							<Icon class="mr-1 shrink-0 text-yellow-500" icon="mdi:warning" />
						</Tooltip>
					{:else}
						<Icon class="mr-1 shrink-0 text-yellow-500" icon="mdi:warning" />
					{/if}
				{/if}
				{#if update}
					{#if isNonReleaseVersion(update.new)}
						<Icon class="text-accent-700 dark:text-accent-200 shrink-0" icon="mdi:flask-outline" />
					{:else}
						<Icon
							class="text-accent-600 dark:text-accent-500 shrink-0"
							icon="mdi:arrow-up-circle"
						/>
					{/if}
				{/if}
				{#if mod.alternateBackend?.reason === 'newer'}
					<Icon
						class="text-accent-600 dark:text-accent-500 mr-1 shrink-0"
						icon="mdi:swap-horizontal"
					/>
					<span class="text-primary-500 dark:text-primary-400 shrink-0 text-xs">
						{m.page_modListItem_newerOnSource({
							version: mod.alternateBackend.latestVersion,
							site: mod.alternateBackend.backend
						})}
					</span>
				{:else if mod.alternateBackend?.reason === 'deprecatedHere'}
					<Icon
						class="text-accent-600 dark:text-accent-500 mr-1 shrink-0"
						icon="mdi:swap-horizontal"
					/>
					<span class="text-primary-500 dark:text-primary-400 shrink-0 text-xs">
						{m.page_modListItem_notDeprecatedOn({ site: mod.alternateBackend.backend })}
					</span>
				{/if}
			</div>

			{#if mod.data.description}
				<div class="text-primary-500 dark:text-primary-400 truncate text-sm">
					{mod.data.description}
				</div>
			{/if}
		</div>
	</div>

	<div class="hidden overflow-hidden lg:block">
		{mod.data.author}
	</div>

	<div>
		{mod.data.version}
	</div>

	{#if trailing}
		{@render trailing()}
	{:else}
		<div></div>
	{/if}
</div>
