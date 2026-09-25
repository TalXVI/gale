import { expect, test } from '@playwright/test';

const item = 'BepInEx/plugins/Author-ModA/SomeMod.dll';

async function hold(page: import('@playwright/test').Page, command: string) {
	await page.evaluate((name) => (window as any).hold(name), command);
}

async function release(page: import('@playwright/test').Page) {
	await page.evaluate(() => (window as any).release());
}

async function emit(page: import('@playwright/test').Page, update: Record<string, unknown>) {
	await page.evaluate((value) => (window as any).emitProgress(value), update);
}

async function runId(page: import('@playwright/test').Page, command: string): Promise<string> {
	return page.evaluate(
		(name) =>
			(window as any).calls.filter((call: any) => call.cmd === name).at(-1).args.request.runId,
		command
	);
}

test('Local Preview and Deploy show snapshot, mutation, byte, and restart progress', async ({
	page
}) => {
	await page.goto('/tests/dialog/?mode=local');
	await hold(page, 'preview_server_sync');
	await page.getByRole('button', { name: 'Preview', exact: true }).click();
	const progress = page.getByTestId('server-sync-operation-progress');
	await expect(progress).toContainText('Previewing server');
	await emit(page, {
		phase: 'scanningPayload',
		completedPhases: 6,
		completed: 15,
		total: null,
		item: 'BepInEx/plugins/Author-ModA'
	});
	await expect(progress).toContainText('Scanning remote payload');
	await expect(progress).toContainText('15 items found');
	await expect(progress.getByRole('progressbar')).toHaveCount(2);
	await expect(
		progress.getByRole('progressbar', { name: 'Scanning remote payload' })
	).not.toHaveAttribute('aria-valuenow', /\d+/);
	await emit(page, { completed: 83, total: 166, item });
	await expect(progress).toContainText('83 / 166 files');
	await expect(
		progress.getByRole('progressbar', { name: 'Verifying deployed mods' })
	).toHaveAttribute('aria-valuenow', '83');
	await expect(progress.getByTitle(item)).toBeVisible();
	const longItem = `BepInEx/plugins/${'deeply-nested-mod/'}${'very-long-file-name-'.repeat(12)}.dll`;
	await emit(page, { completed: 83, total: 166, item: longItem });
	await expect(progress.getByTitle(longItem)).toBeVisible();
	expect(await progress.evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(
		true
	);
	await emit(page, {
		phase: 'buildingPlan',
		completedPhases: 8,
		completed: 0,
		total: null,
		item: null
	});
	await expect(progress).toContainText('Building deployment plan');
	await expect(progress).not.toContainText('83 / 166');
	await release(page);
	await expect(progress).toHaveCount(0);
	await expect(page.getByRole('button', { name: 'Deploy', exact: true })).toBeEnabled();

	await hold(page, 'deploy_server_sync');
	await page.getByRole('button', { name: 'Deploy', exact: true }).click();
	await emit(page, { completed: 83, total: 166, item });
	await expect(progress).toContainText('Verifying deployed mods');
	await emit(page, {
		phase: 'removingFiles',
		completedPhases: 9,
		completed: 3,
		total: 5,
		item: 'BepInEx/plugins/Old/old.dll'
	});
	await expect(progress).toContainText('3 / 5 items');
	await emit(page, {
		phase: 'uploadingPayload',
		completedPhases: 10,
		completed: 12,
		total: 24,
		completedBytes: 18_400_000,
		totalBytes: 52_100_000,
		item
	});
	await expect(progress).toContainText('12 / 24 files · 18.4 MB / 52.1 MB');
	await emit(page, {
		phase: 'applyingRestart',
		completedPhases: 12,
		completed: 0,
		total: null,
		item: 'Waiting for server to stop and start (check 2 of 12)'
	});
	await expect(progress).toContainText('Applying restart policy');
	await expect(progress).toContainText('check 2 of 12');
	await release(page);
	await expect(progress).toHaveCount(0);
	await expect(page.getByText('Mod deployment finished.')).toBeVisible();
});

test('Worker polls its run, ignores stale snapshots, and clears progress on success', async ({
	page
}) => {
	await page.goto('/tests/dialog/?mode=worker');
	await page.evaluate(
		(value) => (window as any).setWorkerProgress({ completed: 83, total: 166, item: value }),
		item
	);
	await hold(page, 'preview_server_sync');
	await page.getByRole('button', { name: 'Preview', exact: true }).click();
	const progress = page.getByTestId('server-sync-operation-progress');
	await expect(progress).toContainText('83 / 166 files');
	const oldRun = await runId(page, 'preview_server_sync');
	await release(page);
	await expect(progress).toHaveCount(0);

	await page.evaluate(
		(value) =>
			(window as any).setWorkerProgress({
				phase: 'verifyingPayload',
				completedPhases: 7,
				completed: 8,
				total: 20,
				item: value
			}),
		item
	);
	await hold(page, 'deploy_server_sync');
	await page.getByRole('button', { name: 'Deploy', exact: true }).click();
	await expect(progress).toContainText('8 / 20 files');
	const polls = await page.evaluate(() => (window as any).progressPolls());
	await page.evaluate(
		(runId) =>
			(window as any).setWorkerProgress({
				runId,
				phase: 'uploadingPayload',
				completedPhases: 10,
				completed: 12,
				total: 24
			}),
		oldRun
	);
	await expect
		.poll(() => page.evaluate(() => (window as any).progressPolls()))
		.toBeGreaterThan(polls);
	await expect(progress).toContainText('8 / 20 files');
	await page.evaluate(
		(value) =>
			(window as any).setWorkerProgress({
				phase: 'uploadingPayload',
				completedPhases: 10,
				completed: 12,
				total: 24,
				completedBytes: 18_400_000,
				totalBytes: 52_100_000,
				item: value
			}),
		item
	);
	await expect(progress).toContainText('12 / 24 files · 18.4 MB / 52.1 MB');
	await release(page);
	await expect(progress).toHaveCount(0);
});

test('failed progress keeps its phase and leaving the tab discards old events', async ({
	page
}) => {
	await page.goto('/tests/dialog/?mode=local');
	await hold(page, 'preview_server_sync');
	await page.evaluate(() => (window as any).fail('preview_server_sync'));
	await page.getByRole('button', { name: 'Preview', exact: true }).click();
	const oldRun = await runId(page, 'preview_server_sync');
	await emit(page, { completed: 42, total: 166, item });
	await release(page);
	const progress = page.getByTestId('server-sync-operation-progress');
	await expect(progress).toContainText('Preview failed while verifying deployed mods');
	await expect(progress).toContainText(`Last item: ${item}`);
	await page.evaluate(() => (window as any).unfail('preview_server_sync'));
	await hold(page, 'preview_server_sync');
	await page.getByRole('button', { name: 'Preview', exact: true }).click();
	await emit(page, { runId: oldRun, completed: 100, total: 166, item: 'stale' });
	await expect(progress).toContainText('Starting operation');
	await emit(page, { completed: 2, total: 3, item: 'current' });
	await expect(progress).toContainText('2 / 3 files');
	await release(page);
	await expect(progress).toHaveCount(0);
	// Switching tabs unmounts the status panel; a stale event emitted while
	// it is gone must not resurrect progress when it remounts.
	await page.getByRole('tab', { name: 'This computer' }).click();
	await emit(page, { runId: oldRun, completed: 100, total: 166, item: 'stale' });
	await page.getByRole('tab', { name: 'Remote server' }).click();
	await expect(progress).toHaveCount(0);
});
