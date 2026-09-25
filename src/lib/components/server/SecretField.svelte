<script lang="ts">
	import InputField from '$lib/components/ui/InputField.svelte';
	import Label from '$lib/components/ui/Label.svelte';
	import { m } from '$lib/paraglide/messages';

	type Props = {
		id: string;
		label: string;
		value?: string;
		/// A credential is already stored — the empty field then shows
		/// dots styled like real text instead of a muted placeholder.
		saved?: boolean;
		disabled?: boolean;
	};

	let { id, label, value = $bindable(''), saved = false, disabled = false }: Props = $props();

	let showDots = $derived(saved && value === '');
	let inputClass = $derived(
		showDots
			? 'mt-1 w-full placeholder:text-primary-700! dark:placeholder:text-primary-300!'
			: 'mt-1 w-full'
	);
</script>

<div>
	<Label for={id}>{label}</Label>
	<InputField
		{id}
		class={inputClass}
		bind:value
		type="password"
		placeholder={showDots ? '••••••••' : undefined}
		aria-describedby={showDots ? `${id}-saved` : undefined}
		{disabled}
	/>
	{#if showDots}
		<span id={`${id}-saved`} class="sr-only">{m.serverPage_credentialSaved()}</span>
	{/if}
</div>
