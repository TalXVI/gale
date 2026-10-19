import { expect, test } from '@playwright/test';
import { createServer, type ViteDevServer } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import { readFile } from 'node:fs/promises';

let server: ViteDevServer;
let fixtureUrl: string;

test.beforeAll(async () => {
	server = await createServer({
		configFile: false,
		cacheDir: 'node_modules/.vite-markdown-security',
		resolve: { dedupe: ['svelte'] },
		plugins: [
			svelte({ configFile: false }),
			{
				name: 'markdown-security-fixture',
				configureServer(vite) {
					vite.middlewares.use(async (request, response, next) => {
						if (request.url?.split('?')[0] !== '/tests/markdown/index.html') return next();
						response.setHeader('Content-Type', 'text/html');
						response.end(await readFile('tests/markdown/index.html', 'utf8'));
					});
				}
			}
		],
		server: { host: '127.0.0.1', port: 0, watch: { ignored: ['**/*'] } },
		optimizeDeps: {
			entries: ['src/lib/components/ui/Markdown.svelte'],
			include: [
				'highlight.js/lib/core',
				'highlight.js/lib/languages/csharp',
				'highlight.js/lib/languages/json',
				'highlight.js/lib/languages/xml'
			]
		},
		logLevel: 'error'
	});
	await server.listen();
	const address = server.httpServer?.address();
	if (!address || typeof address === 'string')
		throw new Error('Markdown fixture did not bind a TCP port');
	fixtureUrl = `http://127.0.0.1:${address.port}`;
});

test.afterAll(async () => {
	await server?.close();
});

test('a README cannot submit a form or keep executable URLs and attributes', async ({ page }) => {
	const exfiltrations: string[] = [];
	await page.route('**/readme-exfil**', async (route) => {
		exfiltrations.push(route.request().url());
		await route.fulfill({ body: 'exfiltration fixture' });
	});
	await page.route('**/missing-markdown-image', (route) =>
		route.fulfill({ status: 404, body: '' })
	);
	const source = `<p><strong>Safe content</strong> <a href="https://example.com/docs">Docs</a></p>
<form action="/readme-exfil" method="get"><input type="hidden" name="secret" value="fixture-secret"><button>Claim prize</button></form>
<a href="javascript:document.body.dataset.injected='yes'">Unsafe link</a>
<img src="/missing-markdown-image" onerror="document.body.dataset.injected='yes'">`;
	await page.goto(`${fixtureUrl}/tests/markdown/index.html#source=${encodeURIComponent(source)}`, {
		waitUntil: 'domcontentloaded'
	});
	await expect(page.getByText('Safe content', { exact: true })).toBeVisible();
	await expect(page.getByRole('link', { name: 'Docs', exact: true })).toHaveAttribute(
		'href',
		'https://example.com/docs'
	);
	await page.getByText('Claim prize', { exact: true }).click();
	await expect(page).toHaveURL(/\/tests\/markdown\//);
	expect(exfiltrations).toEqual([]);
	await expect(page.locator('form, [onerror], [onmouseover], a[href^="javascript:"]')).toHaveCount(
		0
	);
	await expect(page.locator('body')).not.toHaveAttribute('data-injected', 'yes');
});
