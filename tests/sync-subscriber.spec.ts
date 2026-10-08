import { expect, test, type Page } from '@playwright/test';

const paths = {
	custom: 'BepInEx/config/custom.cfg',
	new: 'BepInEx/config/new.cfg',
	deleted: 'BepInEx/config/deleted.cfg'
};

async function calls(page: Page, command: string) {
	return page.evaluate((command) => {
		const entries = Reflect.get(window, 'calls');
		if (!Array.isArray(entries)) throw new Error('Missing IPC calls');
		return entries.filter((entry) => entry.cmd === command);
	}, command);
}

async function openSync(page: Page, query = '') {
	await page.goto(`/tests/dialog/?component=sync${query}`);
	await page.getByRole('button', { name: /Outdated|Up to date/ }).click();
	return page.getByRole('dialog', { name: 'Profile sync', exact: true });
}

test('one click updates mods without a config prompt or config write', async ({
	page
}, testInfo) => {
	const dialog = await openSync(page);
	await expect(
		dialog.getByText('Mod updates keep your configs. Config updates are optional.')
	).toBeVisible();
	await page.screenshot({ path: testInfo.outputPath('subscriber-before-update.png') });
	await dialog.getByRole('button', { name: 'Update mods', exact: true }).click();
	await expect(dialog.getByRole('button', { name: 'Update mods', exact: true })).toHaveCount(0);
	expect(await calls(page, 'pull_sync_profile')).toHaveLength(1);
	expect(await calls(page, 'apply_sync_config')).toHaveLength(0);
	expect(await calls(page, 'plugin:dialog|message')).toHaveLength(0);
	await expect(page.getByRole('dialog', { name: 'Config updates', exact: true })).toHaveCount(0);
	await expect(
		dialog
			.getByRole('region', { name: 'Configs (optional)' })
			.getByRole('button', { name: 'Review config updates (3)' })
	).toBeVisible();
});

test('config review requires selection and replaces only selected files', async ({
	page
}, testInfo) => {
	const dialog = await openSync(page, '&upToDate');
	await expect(dialog.getByRole('button', { name: 'Update mods', exact: true })).toHaveCount(0);
	const reviewButton = dialog.getByRole('button', { name: 'Review config updates (3)' });
	await reviewButton.focus();
	await page.keyboard.press('Enter');
	const review = page.getByRole('dialog', { name: 'Config updates', exact: true });
	await expect(
		review.getByText(
			'Only selected files will replace your local configs. Leave files unchecked to keep your settings.'
		)
	).toBeVisible();
	await page.screenshot({ path: testInfo.outputPath('subscriber-update.png') });
	await expect(review.getByRole('button', { name: 'Apply selected' })).toBeDisabled();
	for (const path of Object.values(paths)) {
		await expect(review.getByRole('checkbox', { name: path, exact: true })).not.toBeChecked();
	}
	await review.getByRole('checkbox', { name: paths.custom, exact: true }).check();
	await review.getByRole('button', { name: 'Apply selected' }).click();
	await expect.poll(async () => (await calls(page, 'apply_sync_config')).length).toBe(1);
	expect((await calls(page, 'apply_sync_config'))[0].args).toEqual({
		files: [paths.custom],
		remember: false,
		restoreDeleted: [],
		profileId: 1
	});
	await expect(review.getByRole('checkbox', { name: paths.custom, exact: true })).toHaveCount(0);
	await expect(review.getByRole('checkbox', { name: paths.new, exact: true })).not.toBeChecked();
	await expect(
		review.getByRole('checkbox', { name: paths.deleted, exact: true })
	).not.toBeChecked();
});

test('new config files can be pulled explicitly without a restore confirmation', async ({
	page
}) => {
	const dialog = await openSync(page, '&upToDate');
	await dialog.getByRole('button', { name: 'Review config updates (3)' }).click();
	const review = page.getByRole('dialog', { name: 'Config updates', exact: true });
	await review.getByRole('checkbox', { name: paths.new, exact: true }).check();
	await review.getByRole('button', { name: 'Apply selected' }).click();
	await expect.poll(async () => (await calls(page, 'apply_sync_config')).length).toBe(1);
	expect((await calls(page, 'apply_sync_config'))[0].args.restoreDeleted).toEqual([]);
	expect(await calls(page, 'plugin:dialog|message')).toHaveLength(0);
});

test('a signed-in publisher uses the same mods-only pull and explicit config selection', async ({
	page
}) => {
	const dialog = await openSync(page, '&authUser&legacyPolicy');
	await dialog.getByRole('button', { name: 'Update mods', exact: true }).click();
	await expect(dialog.getByRole('button', { name: 'Update mods', exact: true })).toHaveCount(0);
	expect(await calls(page, 'pull_sync_profile')).toHaveLength(1);
	expect(await calls(page, 'apply_sync_config')).toHaveLength(0);
	expect(await calls(page, 'plugin:dialog|message')).toHaveLength(0);
	await dialog.getByRole('button', { name: 'Review config updates (3)' }).click();
	const review = page.getByRole('dialog', { name: 'Config updates', exact: true });
	await expect(review.getByRole('button', { name: 'Apply selected' })).toBeDisabled();
	await expect(review.getByText('Remember this choice for future updates')).toHaveCount(0);
	await review.getByRole('checkbox', { name: paths.custom, exact: true }).check();
	await review.getByRole('button', { name: 'Apply selected' }).click();
	await expect.poll(async () => (await calls(page, 'apply_sync_config')).length).toBe(1);
	expect((await calls(page, 'apply_sync_config'))[0].args).toEqual({
		files: [paths.custom],
		remember: false,
		restoreDeleted: [],
		profileId: 1
	});
});

test('restoring deleted configs still requires confirmation', async ({ page }) => {
	const dialog = await openSync(page, '&upToDate');
	await dialog.getByRole('button', { name: 'Review config updates (3)' }).click();
	const review = page.getByRole('dialog', { name: 'Config updates', exact: true });
	await review.getByRole('checkbox', { name: paths.deleted, exact: true }).check();
	await page.evaluate(() => Reflect.get(window, 'answerDialogs')('Cancel'));
	await review.getByRole('button', { name: 'Apply selected' }).click();
	await expect.poll(async () => (await calls(page, 'plugin:dialog|message')).length).toBe(1);
	expect(await calls(page, 'apply_sync_config')).toHaveLength(0);
	await page.evaluate(() => Reflect.get(window, 'answerDialogs')('Ok'));
	await review.getByRole('button', { name: 'Apply selected' }).click();
	await expect.poll(async () => (await calls(page, 'apply_sync_config')).length).toBe(1);
	expect((await calls(page, 'apply_sync_config'))[0].args.restoreDeleted).toEqual([paths.deleted]);
});

for (const account of ['subscriber', 'publisher']) {
	test(`${account} legacy automatic config choices display as review and cannot be re-enabled`, async ({
		page
	}) => {
		const dialog = await openSync(
			page,
			`&upToDate&legacyPolicy${account === 'publisher' ? '&authUser' : ''}`
		);
		await dialog.getByRole('button', { name: 'Config update policies' }).click();
		const policies = page.getByRole('dialog', { name: 'Config update policies', exact: true });
		const row = policies.getByTitle(paths.custom).locator('..');
		await row.getByRole('button', { name: 'Review each update' }).click();
		await expect(page.getByRole('option', { name: 'Always apply future updates' })).toHaveCount(0);
		await page.getByRole('option', { name: 'Always keep my local config' }).click();
		await expect.poll(async () => (await calls(page, 'set_sync_config_policy')).length).toBe(1);
		expect((await calls(page, 'set_sync_config_policy'))[0].args).toEqual({
			file: paths.custom,
			policy: 'alwaysKeep',
			profileId: 1
		});
	});
}
