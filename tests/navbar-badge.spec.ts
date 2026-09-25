import { test, expect, type Page } from '@playwright/test';

const serverLink = (page: Page) => page.locator('nav a[href="/server"]');
const dot = (page: Page) => serverLink(page).locator('span.rounded-full');
const statusCalls = (page: Page) =>
	page.evaluate(() =>
		(window as any).calls.filter((call: any) => call.cmd === 'get_server_sync_status')
	);

test.describe('navbar server badge', () => {
	test('R1: an up-to-date worker remote shows a green dot', async ({ page }) => {
		await page.goto('/tests/dialog/?component=navbar&nopage=1&mode=worker&deployed=1');
		await expect(dot(page)).toHaveClass(/bg-green-500/);
	});

	test('R2: a failed background poll keeps the pending dot', async ({ page }) => {
		await page.clock.install();
		await page.goto('/tests/dialog/?component=navbar&nopage=1&mode=worker&wpending=1');
		await expect(dot(page)).toHaveClass(/bg-amber-500/);

		await page.evaluate(() => (window as any).fail('get_server_sync_status'));
		await page.clock.runFor(60_500);
		// Wait for the failed poll to actually land, then the badge must
		// still show what the last good observation reported.
		await expect.poll(async () => (await statusCalls(page)).length).toBe(2);
		await expect(dot(page)).toHaveClass(/bg-amber-500/);
	});

	test('R3: a degraded live refresh on the page keeps the navbar pending dot', async ({ page }) => {
		await page.clock.install();
		await page.goto('/tests/dialog/?component=navbar&nopage=1&mode=worker&wpending=1');
		await expect(dot(page)).toHaveClass(/bg-amber-500/);

		await page.evaluate(() => {
			(window as any).failWorkerRefresh(true);
			(window as any).showServerPage();
		});
		// The mounted page issues its live refresh; the degraded
		// worker-less response must not clear the navbar's last good
		// worker observation.
		await expect(page.getByRole('tabpanel', { name: 'Remote server' })).toBeVisible();
		await expect.poll(async () => (await statusCalls(page)).length).toBeGreaterThan(1);
		await expect(dot(page)).toHaveClass(/bg-amber-500/);
	});

	test('R4: a stale poll landing after a profile switch does not badge it', async ({ page }) => {
		await page.clock.install();
		await page.goto('/tests/dialog/?component=navbar&nopage=1&mode=worker&wpending=1&holdStatus=1');
		// Profile 1's initial navbar poll is in flight; profile 2 is a
		// manual-sync profile that must never show a remote badge.
		await expect(serverLink(page)).toBeVisible();
		await page.evaluate(() => (window as any).switchProfile(2));
		await page.evaluate(() => (window as any).release());
		await expect.poll(async () => (await statusCalls(page)).length).toBe(1);
		await expect(dot(page)).toHaveCount(0);
	});

	test('a stale in-flight poll does not block the new profile’s first poll', async ({ page }) => {
		await page.clock.install();
		await page.goto('/tests/dialog/?component=navbar&nopage=1&mode=worker&deployed=1&worker2=1');
		await expect(dot(page)).toHaveClass(/bg-green-500/);

		// Park profile 1's next tick mid-flight, then switch: profile 2's
		// immediate poll must still be issued while it sits held.
		await page.evaluate(() => (window as any).hold('get_server_sync_status'));
		await page.clock.runFor(60_500);
		await expect.poll(async () => (await statusCalls(page)).length).toBe(2);

		await page.evaluate(() => (window as any).switchProfile(2));
		await expect.poll(async () => (await statusCalls(page)).length).toBe(3);
		await page.evaluate(() => (window as any).release());

		// Profile 2's own observation badges it amber — profile 1's stale
		// green answer is discarded and never comes back.
		await expect(dot(page)).toHaveClass(/bg-amber-500/);
		await expect(dot(page)).not.toHaveClass(/bg-green-500/);
	});

	test('switching to a non-worker profile clears an established badge', async ({ page }) => {
		await page.goto('/tests/dialog/?component=navbar&nopage=1&mode=worker&wpending=1');
		await expect(dot(page)).toHaveClass(/bg-amber-500/);

		await page.evaluate(() => (window as any).switchProfile(2));
		await expect(dot(page)).toHaveCount(0);
	});

	test('a running local server wins over a pending remote', async ({ page }) => {
		await page.goto('/tests/dialog/?component=navbar&nopage=1&mode=worker&wpending=1&running=1');
		await expect(dot(page)).toHaveClass(/bg-green-500/);
	});

	test('the server nav icon does not change when the link is active', async ({ page }) => {
		// The visible (non-outline-variant) icon — Iconify renders it async.
		const iconHtml = () =>
			serverLink(page)
				.locator('svg:not(.hidden)')
				.evaluate((el) => el.outerHTML);
		await page.goto('/tests/dialog/?component=navbar&nopage=1&path=/server');
		await expect.poll(iconHtml).toContain('<svg');
		const onServer = await iconHtml();
		await page.goto('/tests/dialog/?component=navbar&nopage=1&path=/');
		await expect.poll(iconHtml).toContain('<svg');
		expect(await iconHtml()).toBe(onServer);
	});
});
