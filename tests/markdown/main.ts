import { mount } from 'svelte';
import Markdown from '../../src/lib/components/ui/Markdown.svelte';

mount(Markdown, {
	target: document.body,
	props: { source: new URLSearchParams(window.location.hash.slice(1)).get('source') ?? '' }
});
