<script lang="ts">
	import InputField from '$lib/components/ui/InputField.svelte';
	import Checkbox from '$lib/components/ui/Checkbox.svelte';
	import Label from '$lib/components/ui/Label.svelte';
	import Info from '$lib/components/ui/Info.svelte';
	import { m } from '$lib/paraglide/messages';

	type Props = {
		id: string;
		label: string;
		value?: string;
		/// A credential is already stored — the empty field then shows
		/// dots styled like real text instead of a muted placeholder.
		saved?: boolean;
		disabled?: boolean;
		/// Label of a "Remember …" checkbox rendered directly under the
		/// input. When given, `remember` binds to it.
		rememberLabel?: string;
		remember?: boolean;
	};

	let {
		id,
		label,
		value = $bindable(''),
		saved = false,
		disabled = false,
		rememberLabel,
		remember = $bindable(true)
	}: Props = $props();

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
	{#if rememberLabel}
		<div class="mt-1 flex items-center">
			<Label for={`${id}-remember`}>{rememberLabel}</Label><Info>{m.serverPage_rememberInfo()}</Info
			><Checkbox id={`${id}-remember`} bind:checked={remember} />
		</div>
	{/if}
</div>
