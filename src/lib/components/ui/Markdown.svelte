<script lang="ts">
	import type { Plugin } from 'svelte-exmarkdown';
	import Markdown from 'svelte-exmarkdown';
	import { gfmPlugin } from 'svelte-exmarkdown/gfm';
	import rehypeRaw from 'rehype-raw';
	import rehypeSanitize, { defaultSchema } from 'rehype-sanitize';
	import 'highlight.js/styles/atom-one-dark.css';
	import csharp from 'highlight.js/lib/languages/csharp';
	import json from 'highlight.js/lib/languages/json';
	import xml from 'highlight.js/lib/languages/xml';
	import rehypeHighlight from 'rehype-highlight';
	import type { ClassValue } from 'clsx';

	type Props = {
		source: string;
		class?: ClassValue;
	};

	let { source, class: classProp }: Props = $props();

	const plugins: Plugin[] = [
		gfmPlugin(),
		{ rehypePlugin: [rehypeRaw] },
		{
			rehypePlugin: [
				rehypeSanitize,
				{
					...defaultSchema,
					tagNames: defaultSchema.tagNames?.concat('button'),
					// Preserve button labels without allowing form submission.
					required: { ...defaultSchema.required, button: { type: 'button' } }
				}
			]
		},
		{
			rehypePlugin: [rehypeHighlight, { languages: { csharp, json, xml }, ignoreMissing: true }]
		}
	];
</script>

<div class={[classProp, 'markdown overflow-x-hidden']}>
	<Markdown md={source} {plugins}>
		{#snippet img({ class: classProp, ...props })}
			<img {...props} class={['m-0', classProp]} />
		{/snippet}
		{#snippet a({ children, ...props })}
			<a {...props} target="_blank" rel="noreferrer nofollow">
				{@render children?.()}
			</a>
		{/snippet}
	</Markdown>
</div>
