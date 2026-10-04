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
