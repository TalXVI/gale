<script lang="ts">
	import Icon from '@iconify/svelte';
	import { page } from '$app/state';
	import Tooltip from '$lib/components/ui/Tooltip.svelte';

	type Props = {
		to: string;
		icon: string;
		tooltip: string;
		disabled?: boolean;
		outline?: boolean;
		/// Status dot: green when the local server runs or the remote is
		/// up to date, amber when the remote has an update pending.
		badge?: 'healthy' | 'pending';
	};

	let { to, icon, tooltip, outline = true, disabled = false, badge }: Props = $props();

	let active = $derived(page.url.pathname === to);
	let hasOutline = $derived(outline && !active);

	const commonClasses = 'relative flex items-center rounded-lg p-2.5 text-3xl';
</script>

<Tooltip text={tooltip} side="right">
	{#if disabled}
		<button
			disabled
			class={[
				commonClasses,
				'text-primary-300 dark:text-primary-500 cursor-not-allowed opacity-50'
			]}
		>
			{@render icon_()}
		</button>
	{:else}
		<a
			href={to}
			class={[
				active
					? 'text-accent-500 dark:bg-primary-800 bg-primary-200 font-semibold'
					: 'text-primary-400 hover:text-primary-500 dark:text-primary-500 dark:hover:bg-primary-800 dark:hover:text-primary-400 hover:bg-primary-200',
				commonClasses
			]}
		>
			{@render icon_()}
		</a>
	{/if}
</Tooltip>

{#snippet icon_()}
	<Icon {icon} class={[hasOutline && 'hidden']} />
	<Icon icon="{icon}-outline" class={[!hasOutline && 'hidden']} />
	{#if badge}
		<span
			data-badge={badge}
			class={[
				'absolute top-1 right-1 size-2 rounded-full',
				badge === 'healthy' ? 'bg-green-500' : 'bg-amber-500'
			]}
		></span>
	{/if}
{/snippet}
