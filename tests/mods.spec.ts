import { expect, test } from '@playwright/test';

test('unknown-mod uninstall requests every unknown UUID', async ({ page }) => {
	await page.goto('/tests/mods/');
	await page.getByRole('button', { name: 'Details', exact: true }).click();
	await page.getByRole('button', { name: 'Uninstall', exact: true }).click();
	await expect
		.poll(() =>
			page.evaluate(() =>
				(window as any).calls.find((call: any) => call.cmd === 'force_remove_mods')
			)
		)
		.toMatchObject({
			args: {
				uuids: ['00000000-0000-0000-0000-000000000001', '00000000-0000-0000-0000-000000000003']
			}
		});
});

test('disabled local-mod icons use the disabled file in the row and details', async ({ page }) => {
	await page.goto('/tests/mods/');
	await expect(page.getByAltText('LocalMod')).toHaveAttribute('src', /icon\.png\.old$/);
	await page.locator('div[role=button]').filter({ hasText: 'LocalMod' }).last().click();
	await expect(page.locator('img')).toHaveCount(2);
	for (const icon of await page.locator('img').all()) {
		await expect(icon).toHaveAttribute('src', /icon\.png\.old$/);
	}
});

test('installed-mod details retain the direct config-editor action', async ({ page }) => {
	await page.goto('/tests/mods/');
	await page.locator('div[role=button]').filter({ hasText: 'LocalMod' }).last().click();
	await page.getByRole('button', { name: 'Edit config', exact: true }).click();
	await expect.poll(() => page.evaluate(() => (window as any).navigations)).toEqual(['/config']);
});

for (const [ts, hx, preferred] of [
	['1.2.3-beta', '1.2.4', 'Hexium'],
	['1.2.3-beta', '1.2.3', 'Hexium'],
	['1.2.3-beta2', '1.2.3-beta10', 'Thunderstore'],
	['1.2.3-beta.2', '1.2.3-beta.10', 'Hexium'],
	['1.2.3+build.1', '1.2.3+build.2', 'Thunderstore']
]) {
	test(`source preference follows semver for ${ts} versus ${hx}`, async ({ page }) => {
		await page.goto(
			`/tests/mods/?browse&ts=${encodeURIComponent(ts)}&hx=${encodeURIComponent(hx)}`
		);
		await page.locator('div[role=button]').filter({ hasText: 'DualSourceMod' }).click();
		await expect(page.getByRole('tab', { name: preferred, exact: true })).toHaveAttribute(
			'aria-selected',
			'true'
		);
	});
}

test('source tabs install the selected source and version', async ({ page }) => {
	await page.goto('/tests/mods/?browse');
	await page.locator('div[role=button]').filter({ hasText: 'DualSourceMod' }).click();
	await page.getByRole('tab', { name: 'Thunderstore', exact: true }).click();
	await page.getByRole('button', { name: 'Install', exact: true }).click();
	await expect
		.poll(() =>
			page.evaluate(() => (window as any).calls.find((call: any) => call.cmd === 'install_mod'))
		)
		.toMatchObject({
			args: {
				id: {
					backend: 'Thunderstore',
					versionUuid: '00000000-0000-0000-0000-000000000010'
				}
			}
		});
});

test('a running dedicated server keeps mod installation locked', async ({ page }) => {
	await page.goto('/tests/mods/?browse&locked');
	await page.locator('div[role=button]').filter({ hasText: 'DualSourceMod' }).click();
	await expect(page.getByRole('button', { name: 'Profile locked', exact: true })).toBeDisabled();
});

test('Pull live refreshes the selected installed mod without installing it', async ({
	page
}, testInfo) => {
	await page.goto('/tests/mods/?live');
	const row = page.locator('div[role=button]').filter({ hasText: 'DualSourceMod' }).last();
	await row.click({ button: 'right' });
	await expect(page.getByRole('menuitem', { name: 'Pull live', exact: true })).toBeVisible();
	await page.screenshot({
		path: testInfo.outputPath('pull-live-menu.png'),
		animations: 'disabled'
	});
	await page.getByRole('menuitem', { name: 'Pull live', exact: true }).click();
	await expect(
		page.getByText('Pulled live metadata for DualSourceMod. Latest version: 1.1.4.')
	).toBeVisible();
	await expect
		.poll(() =>
			page.evaluate(() => (window as any).calls.filter((call: any) => call.cmd === 'pull_live_mod'))
		)
		.toEqual([
			{
				cmd: 'pull_live_mod',
				args: { packageUuid: '00000000-0000-0000-0000-000000000002', game: 'valheim' }
			}
		]);
	await row.click();
	await expect(page.getByRole('button', { name: /Update/ }).first()).toBeVisible();
	await expect
		.poll(() =>
			page.evaluate(() =>
				(window as any).calls.some((call: any) =>
					['install_mod', 'update_mods', 'trigger_mod_fetch'].includes(call.cmd)
				)
			)
		)
		.toBe(false);
});

test('Pull live is absent for local mods and Hexium sources', async ({ page }) => {
	await page.goto('/tests/mods/');
	await page
		.locator('div[role=button]')
		.filter({ hasText: 'LocalMod' })
		.last()
		.click({ button: 'right' });
	await expect(page.getByRole('menuitem', { name: 'Pull live', exact: true })).toHaveCount(0);
	await page.goto('/tests/mods/?browse');
	await page
		.locator('div[role=button]')
		.filter({ hasText: 'DualSourceMod' })
		.click({ button: 'right' });
	await expect(page.getByRole('menuitem', { name: 'Pull live', exact: true })).toHaveCount(0);
});

test('Pull live refreshes browse metadata for the selected Thunderstore source', async ({
	page
}) => {
	await page.goto('/tests/mods/?browse&ts=1.1.3&hx=1.0.0');
	await page
		.locator('div[role=button]')
		.filter({ hasText: 'DualSourceMod' })
		.click({ button: 'right' });
	await page.getByRole('menuitem', { name: 'Pull live', exact: true }).click();
	await expect(
		page.getByText('Pulled live metadata for DualSourceMod. Latest version: 1.1.4.')
	).toBeVisible();
	await page.locator('div[role=button]').filter({ hasText: 'DualSourceMod' }).click();
	await expect(page.getByText('1.1.4', { exact: true }).first()).toBeVisible();
});

test('Pull live reports dependency errors and leaves the current update state alone', async ({
	page
}) => {
	await page.goto('/tests/mods/?live&liveError');
	const row = page.locator('div[role=button]').filter({ hasText: 'DualSourceMod' }).last();
	await row.click({ button: 'right' });
	await page.getByRole('menuitem', { name: 'Pull live', exact: true }).click();
	await expect(
		page.getByText(/Required dependency Author-Loader-2.0.0 is not available/)
	).toBeVisible();
	await row.click();
	await expect(page.getByRole('button', { name: /Update/ })).toHaveCount(0);
	await expect(page.getByText('1.1.3', { exact: true }).first()).toBeVisible();
});

test('repeated Pull live clicks share the in-flight request', async ({ page }) => {
	await page.goto('/tests/mods/?live&liveDelay');
	const row = page.locator('div[role=button]').filter({ hasText: 'DualSourceMod' }).last();
	for (let i = 0; i < 2; i++) {
		await row.click({ button: 'right' });
		await page.getByRole('menuitem', { name: 'Pull live', exact: true }).click();
	}
	await expect(
		page.getByText('Pulled live metadata for DualSourceMod. Latest version: 1.1.4.').first()
	).toBeVisible();
	await expect
		.poll(() =>
			page.evaluate(
				() => (window as any).calls.filter((call: any) => call.cmd === 'pull_live_mod').length
			)
		)
		.toBe(1);
});
