<script lang="ts">
	import type { Snippet } from 'svelte';
	import type { HTMLLabelAttributes } from 'svelte/elements';

	type Props = {
		children?: Snippet;
		/// Compact settings keep the label at its natural width so an info
		/// tooltip and checkbox/select sit right next to it, instead of
		/// reserving the 35% form-field column.
		inline?: boolean;
	} & HTMLLabelAttributes;

	let { children, inline = false, class: classProp, ...props }: Props = $props();
</script>

<label
	{...props}
	class={[
		'text-primary-700 dark:text-primary-300 cursor-default truncate text-left',
		{ 'w-[35%] min-w-52': !inline },
		classProp
	]}
>
	{@render children?.()}
</label>
