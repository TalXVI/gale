import { test, expect } from '@playwright/test';

test('local settings save includes the password and its remember choice', async ({ page }) => {
	await page.goto('/tests/dialog/?setup=1');
	await page.getByLabel('Server name', { exact: true }).fill('Friends server');
	await page.getByLabel('World', { exact: true }).fill('Friends');
	await page
		.getByRole('tabpanel', { name: 'This computer' })
		.getByLabel('Password', { exact: true })
		.fill('test-game-password');
	await page.getByRole('button', { name: 'Save settings', exact: true }).click();
	await expect(page.getByRole('button', { name: 'Save settings', exact: true })).toBeEnabled();
	const saved = () =>
		page.evaluate(
			() =>
				(window as any).calls
					.filter((call: any) => call.cmd === 'set_dedicated_server_settings')
					.at(-1).args.request
		);
	expect(await saved()).toMatchObject({
		gamePassword: 'test-game-password',
		rememberGamePassword: true
	});
	await page
		.getByRole('tabpanel', { name: 'This computer' })
		.getByLabel('Remember password', { exact: true })
		.uncheck();
	await expect(page.getByText('With Remember password off', { exact: false })).toBeVisible();
	await page.getByRole('button', { name: 'Save settings', exact: true }).click();
	await expect(page.getByRole('button', { name: 'Save settings', exact: true })).toBeEnabled();
	expect(await saved()).toMatchObject({
		gamePassword: 'test-game-password',
		rememberGamePassword: false
	});
});

test('connection tests and Cancel do not submit a settings save', async ({ page }) => {
	await page.goto('/tests/dialog/?setup=1');
	await page.getByRole('tab', { name: 'Remote server' }).click();
	await page.getByLabel('Host', { exact: true }).fill('example.test');
	await page.getByLabel('Username', { exact: true }).fill('test-user');
	await page
		.getByRole('tabpanel', { name: 'Remote server' })
		.getByLabel('Password', { exact: true })
		.fill('test-remote-password');
	await page.getByRole('button', { name: 'Test connection', exact: true }).click();
	await expect(page.getByRole('button', { name: 'Test connection', exact: true })).toBeEnabled();
	await page.getByLabel('Sync mode', { exact: true }).click();
	await page.getByRole('option', { name: 'Worker on another machine', exact: true }).click();
	await page.getByLabel('Worker address', { exact: true }).fill('https://worker.example.test');
	await page.getByLabel('Worker token', { exact: true }).fill('test-token');
	await page.getByRole('button', { name: 'Test worker', exact: true }).click();
	await expect(page.getByRole('button', { name: 'Test worker', exact: true })).toBeEnabled();
	await page.getByRole('button', { name: 'Cancel', exact: true }).click();
	const commands = await page.evaluate(() => (window as any).calls.map((call: any) => call.cmd));
	expect(commands).toContain('test_remote_server_connection');
	expect(commands).toContain('test_worker_connection');
	expect(commands).not.toContain('set_dedicated_server_settings');
	await page.getByRole('button', { name: 'Reopen sync dialog' }).click();
	await page.getByRole('tab', { name: 'Remote server' }).click();
	await expect(page.getByLabel('Host', { exact: true })).toHaveValue('');
	await expect(
		page.getByRole('tabpanel', { name: 'Remote server' }).getByLabel('Password', { exact: true })
	).toHaveValue('');
	await expect(page.getByLabel('Sync mode', { exact: true })).toHaveText('Manual sync with Gale');
});

for (const cancel of [true, false]) {
	test(`running server has a guarded force-stop action (cancel=${cancel})`, async ({ page }) => {
		await page.goto(`/tests/dialog/?setup=1&running=1${cancel ? '&cancelStop=1' : ''}`);
		await expect(page.getByRole('button', { name: 'Launch server', exact: true })).toBeDisabled();
		await page.getByRole('button', { name: 'Force stop server', exact: true }).click();
		if (cancel)
			await expect(page.getByRole('button', { name: 'Launch server', exact: true })).toBeDisabled();
		else {
			await expect(
				page.getByRole('button', { name: 'Force stop server', exact: true })
			).toHaveCount(0);
			await expect(page.getByRole('button', { name: 'Launch server', exact: true })).toBeEnabled();
		}
		const calls = await page.evaluate(() => (window as any).calls);
		expect(calls.some((call: any) => call.cmd === 'force_stop_dedicated_server')).toBe(!cancel);
		expect(calls.find((call: any) => call.cmd === 'plugin:dialog|message').args.message).toContain(
			'Unsaved world progress may be lost'
		);
	});
}

test('deployment actions stay inside the dialog at the default app size', async ({ page }) => {
	await page.setViewportSize({ width: 900, height: 700 });
	await page.goto('/tests/dialog/?many=1');
	await page.getByText('Server config files', { exact: true }).click();
	await page.getByRole('button', { name: 'Preview config changes', exact: true }).click();
	await page
		.getByTestId('server-config-row')
		.first()
		.getByRole('button', { name: /^Apply / })
		.click();
	await expect(page.getByText('Selection changed. Preview again before deploying.')).toBeVisible();
	const panel = page.getByRole('dialog').locator('div.border').first();
	const bounds = await panel.boundingBox();
	expect(bounds).not.toBeNull();
	for (const name of ['Close', 'Preview', 'Push configs']) {
		const button = page.getByRole('button', { name, exact: true });
		await button.scrollIntoViewIfNeeded();
		const box = (await button.boundingBox())!;
		expect(box.x).toBeGreaterThanOrEqual(bounds!.x);
		expect(box.x + box.width).toBeLessThanOrEqual(bounds!.x + bounds!.width);
		expect(box.y + box.height).toBeLessThanOrEqual(700);
	}
});

test('future config policies do not imply a current deployment decision', async ({ page }) => {
	await page.goto('/tests/dialog/');
	await page.getByText('Server config files', { exact: true }).click();
	await page.getByRole('button', { name: 'Preview config changes', exact: true }).click();
	await expect(page.getByText('This deployment:', { exact: false })).toBeVisible();
	await page.getByLabel('Future updates for BepInEx/config/test.cfg', { exact: true }).click();
	await page.getByRole('option', { name: 'Always keep my config', exact: true }).click();
	await expect(page.getByText('1 files still need a decision')).toBeVisible();
	await expect(page.getByRole('button', { name: 'Push configs', exact: true })).toBeDisabled();
	await page.getByRole('button', { name: 'Preview config changes', exact: true }).click();
	const selection = await page.evaluate(
		() =>
			(window as any).calls.filter((call: any) => call.cmd === 'preview_server_sync').at(-1).args
				.request.selection
	);
	expect(selection.applyConfigs).toEqual([]);
	expect(selection.declineConfigs).toEqual([]);
});
