<script lang="ts">
	import type { Snippet } from 'svelte';
	import type { HTMLLabelAttributes } from 'svelte/elements';

	type Props = {
		children?: Snippet;
	} & HTMLLabelAttributes;

	let { children, class: classProp, ...props }: Props = $props();
</script>

<label
	{...props}
	class={[
		'text-primary-700 dark:text-primary-300 w-[35%] min-w-52 cursor-default truncate text-left',
		classProp
	]}
>
	{@render children?.()}
</label>
