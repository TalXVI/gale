import { expect, test } from '@playwright/test';

const paths = {
	new: 'BepInEx/config/new.cfg',
	modified: 'BepInEx/config/modified.cfg',
	published: 'BepInEx/config/published.cfg',
	removed: 'BepInEx/config/ArgusMagnus.ServersideQoL.cfg'
};

test('removed configs need selection, and Select changes includes every change', async ({
	page
}) => {
	await page.goto('/tests/dialog/?component=publish');
	const dialog = page.getByRole('dialog', { name: 'Publish update' });
	const row = (path: string) => dialog.getByTitle(path).locator('..');
	const checkbox = (path: string) => row(path).getByRole('checkbox');

	await expect(row(paths.new).getByText('new', { exact: true })).toBeVisible();
	await expect(row(paths.modified).getByText('modified', { exact: true })).toBeVisible();
	await expect(row(paths.published).getByText('published', { exact: true })).toBeVisible();
	await expect(row(paths.removed).getByText('removed', { exact: true })).toBeVisible();
	await expect(row(paths.removed).getByText('0 B')).toHaveCount(0);
	await expect(checkbox(paths.removed)).not.toBeChecked();

	await dialog.getByRole('button', { name: 'Clear' }).click();
	await dialog.getByRole('button', { name: 'Select changes' }).click();
	for (const status of ['new', 'modified', 'removed'] as const) {
		await expect(checkbox(paths[status])).toBeChecked();
	}
	await expect(checkbox(paths.published)).not.toBeChecked();

	await checkbox(paths.modified).uncheck();
	await dialog.getByRole('button', { name: 'Publish' }).click();
	const call = await page.evaluate(() =>
		(window as any).calls.findLast((entry: any) => entry.cmd === 'push_sync_profile')
	);
	expect(call.args.mode).toEqual({
		kind: 'both',
		files: [paths.new],
		removeFiles: [paths.removed]
	});
});

test('publishing without selecting a removed config keeps it out of removals', async ({ page }) => {
	await page.goto('/tests/dialog/?component=publish');
	const dialog = page.getByRole('dialog', { name: 'Publish update' });
	await expect(dialog.getByText('removed', { exact: true })).toBeVisible();
	await dialog.getByRole('button', { name: 'Publish' }).click();
	const call = await page.evaluate(() =>
		(window as any).calls.findLast((entry: any) => entry.cmd === 'push_sync_profile')
	);
	expect(call.args.mode.removeFiles).toEqual([]);
});

test('configs-only publication accepts a removal without local writes', async ({ page }) => {
	await page.goto('/tests/dialog/?component=publish');
	const dialog = page.getByRole('dialog', { name: 'Publish update' });
	await expect(dialog.getByText('removed', { exact: true })).toBeVisible();
	await dialog.getByRole('button', { name: 'Mods and configs' }).click();
	await page.getByRole('option', { name: 'Configs only' }).click();
	await dialog.getByRole('button', { name: 'Clear' }).click();
	await dialog.getByTitle(paths.removed).locator('..').getByRole('checkbox').check();
	await dialog.getByRole('button', { name: 'Publish' }).click();
	const call = await page.evaluate(() =>
		(window as any).calls.findLast((entry: any) => entry.cmd === 'push_sync_profile')
	);
	expect(call.args.mode).toEqual({ kind: 'config', files: [], removeFiles: [paths.removed] });
});
