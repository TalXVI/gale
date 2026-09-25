// Test double for SvelteKit's `$app/state` — the harness serves a plain
// Vite page, so `page` is shaped from the URL the test navigated to.
const params = new URLSearchParams(location.search);

export const page = $state({
	url: new URL(location.origin + (params.get('path') ?? '/server'))
});
