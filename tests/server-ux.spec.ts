import { test, expect } from '@playwright/test';

test('local settings save includes the password and its remember choice', async ({ page }) => {
	await page.goto('/tests/dialog/?local=1');

	const localTab = page.getByRole('tabpanel', { name: 'This computer' });
	await localTab.getByLabel('Server name').fill('Friends server');
	await localTab.getByLabel('World').fill('Friends');
	await localTab.getByLabel('Password', { exact: true }).fill('test-game-password');
	await localTab.getByLabel('Server port').fill('2458');
	await page.getByRole('button', { name: 'Save settings' }).waitFor({ state: 'visible' });
	await page.getByRole('button', { name: 'Save settings' }).click();
	await expect(localTab.getByRole('button', { name: 'Launch server' })).toBeEnabled();
	const first = await page.evaluate(
		() =>
			(window as any).calls
				.filter((call: any) => call.cmd === 'set_dedicated_server_settings')
				.at(-1).args.request
	);
	expect(first).toMatchObject({
		gamePassword: 'test-game-password',
		rememberGamePassword: true
	});
	expect(first.settings.serverName).toBe('Friends server');
	expect(first.settings.world).toBe('Friends');

	await localTab.getByLabel('Remember password').uncheck();
	await expect(localTab.getByText('With Remember password off,')).toBeVisible();
	await localTab.getByLabel('Password', { exact: true }).fill('test-game-password');
	await page.getByRole('button', { name: 'Save settings' }).click();
	await expect(localTab.getByText('With Remember password off,')).toBeVisible();
	const second = await page.evaluate(
		() =>
			(window as any).calls
				.filter((call: any) => call.cmd === 'set_dedicated_server_settings')
				.at(-1).args.request
	);
	expect(second).toMatchObject({
		gamePassword: 'test-game-password',
		rememberGamePassword: false
	});
});

test('connection tests run against unsaved input and Discard restores the loaded values', async ({
	page
}) => {
	await page.goto('/tests/dialog/?unset=1');
	await page.getByRole('tab', { name: 'Remote server' }).click();
	const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
	await remoteTab.getByLabel('Host', { exact: true }).fill('example.test');
	await remoteTab.getByLabel('Username').fill('test-user');
	await remoteTab.getByLabel('Password', { exact: true }).fill('test-remote-password');
	await remoteTab.getByLabel('Sync mode').click();
	await page.getByRole('option', { name: 'Worker on another machine' }).click();
	await remoteTab.getByLabel('Worker address').fill('https://worker.example.test');
	await remoteTab.getByLabel('Worker token').fill('test-token');
	await remoteTab.getByRole('button', { name: 'Test connection' }).click();
	await expect(remoteTab.getByRole('button', { name: 'Test connection' })).toBeEnabled();
	await remoteTab.getByRole('button', { name: 'Test worker' }).click();
	await expect(remoteTab.getByRole('button', { name: 'Test worker' })).toBeEnabled();
	expect(await page.evaluate(() => (window as any).calls.map((call: any) => call.cmd))).toEqual(
		expect.arrayContaining(['test_remote_server_connection', 'test_worker_connection'])
	);
	const connectionTest = await page.evaluate(
		() =>
			(window as any).calls.find((call: any) => call.cmd === 'test_remote_server_connection').args
				.request
	);
	expect(connectionTest.settings.host).toBe('example.test');
	expect(connectionTest.password).toBe('test-remote-password');
	const workerTest = await page.evaluate(
		() =>
			(window as any).calls.find((call: any) => call.cmd === 'test_worker_connection').args.request
	);
	expect(workerTest.workerToken).toBe('test-token');
	expect(
		await page.evaluate(() =>
			(window as any).calls.some((call: any) => call.cmd === 'set_dedicated_server_settings')
		)
	).toBe(false);

	// The loaded snapshot is the local defaults: Discard snaps the tab back.
	await page.getByRole('button', { name: 'Discard' }).click();
	await page.getByRole('tab', { name: 'Remote server' }).click();
	await expect(remoteTab.getByLabel('Host', { exact: true })).toHaveValue('');
	await expect(remoteTab.getByLabel('Sync mode')).toHaveText('Manual sync with Gale');
});

test('switching tabs does not mark the form dirty', async ({ page }) => {
	await page.goto('/tests/dialog/?local=1&status=upToDate');
	await expect(page.getByText('Unsaved changes')).toHaveCount(0);
	await page.getByRole('tab', { name: 'Remote server' }).click();
	await expect(page.getByText('Up to date')).toBeVisible();
	await expect(page.getByText('Unsaved changes')).toHaveCount(0);
	await page.getByRole('tab', { name: 'This computer' }).click();
	await expect(page.getByText('Unsaved changes')).toHaveCount(0);
});

test('an unconfigured remote shows setup guidance and never polls status', async ({ page }) => {
	await page.goto('/tests/dialog/?unset=1');
	await page.getByRole('tab', { name: 'Remote server' }).click();
	const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
	await expect(remoteTab.getByText('Not set up yet')).toBeVisible();
	await expect(
		remoteTab.getByText('Enter the connection details below and save to start deploying.')
	).toBeVisible();
	// The status/deploy panels are not mounted, so no sync-status call can
	// have been issued. Give any stray poll a moment to betray itself.
	await page.waitForTimeout(300);
	expect(
		await page.evaluate(
			() =>
				(window as any).calls.filter((call: any) => call.cmd === 'get_server_sync_status').length
		)
	).toBe(0);
});

test('deploy stays disabled while settings are dirty', async ({ page }) => {
	await page.goto('/tests/dialog/?status=upToDate');
	const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
	await expect(remoteTab.getByText('Up to date')).toBeVisible();
	await remoteTab.getByLabel('Host', { exact: true }).fill('changed.example.test');
	await expect(remoteTab.getByText('Save or discard your changes before deploying.')).toBeVisible();
	await expect(remoteTab.getByRole('button', { name: 'Preview' })).toBeDisabled();
});

test('toggling Remember passwords is a change: dirty, restorable, and saved', async ({ page }) => {
	await page.goto('/tests/dialog/?status=upToDate');
	const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
	const remember = remoteTab.getByLabel('Remember passwords');
	await expect(remember).toBeChecked();

	await remember.uncheck();
	await expect(page.getByText('Unsaved changes')).toBeVisible();

	// Discard restores the checkbox and keeps the current tab.
	await page.getByRole('button', { name: 'Discard' }).click();
	await expect(remember).toBeChecked();
	await expect(page.getByText('Unsaved changes')).toHaveCount(0);
	await expect(remoteTab.getByText('Up to date')).toBeVisible();

	await remember.uncheck();
	await page.getByRole('button', { name: 'Save settings' }).click();
	const save = await page.evaluate(
		() =>
			(window as any).calls
				.filter((call: any) => call.cmd === 'set_dedicated_server_settings')
				.at(-1).args.request
	);
	expect(save.rememberCredentials).toBe(false);
	await expect(page.getByText('Unsaved changes')).toHaveCount(0);
});

test.describe('saved credentials', () => {
	test('stored remote password shows dots and stays stored when untouched', async ({ page }) => {
		await page.goto('/tests/dialog/?saved=1');
		const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
		const password = remoteTab.getByLabel('Password', { exact: true });
		await expect(password).toHaveAttribute('placeholder', '••••••••');

		await remoteTab.getByLabel('Username').fill('other-user');
		await page.getByRole('button', { name: 'Save settings' }).click();
		const save = await page.evaluate(
			() =>
				(window as any).calls
					.filter((call: any) => call.cmd === 'set_dedicated_server_settings')
					.at(-1).args.request
		);
		expect(save.remotePassword).toBe('');
		await expect(password).toHaveAttribute('placeholder', '••••••••');
	});

	test('a typed credential replaces the saved one and returns to dots after saving', async ({
		page
	}) => {
		await page.goto('/tests/dialog/?saved=1');
		const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
		const password = remoteTab.getByLabel('Password', { exact: true });
		await password.fill('new-secret');
		await page.getByRole('button', { name: 'Save settings' }).click();
		const save = await page.evaluate(
			() =>
				(window as any).calls
					.filter((call: any) => call.cmd === 'set_dedicated_server_settings')
					.at(-1).args.request
		);
		expect(save.remotePassword).toBe('new-secret');
		await expect(password).toHaveAttribute('placeholder', '••••••••');
	});
});

test.describe('status line', () => {
	test('up-to-date publication', async ({ page }) => {
		await page.goto('/tests/dialog/?status=upToDate');
		await expect(page.getByText('Up to date')).toBeVisible();
	});

	test('newer publication', async ({ page }) => {
		await page.goto('/tests/dialog/?status=pending');
		await expect(page.getByText('Newer publication available')).toBeVisible();
	});

	test('never deployed', async ({ page }) => {
		await page.goto('/tests/dialog/?status=never');
		await expect(page.getByText('Never deployed')).toBeVisible();
	});

	test('worker update pending', async ({ page }) => {
		await page.goto('/tests/dialog/?mode=worker');
		await page.evaluate(() => (window as any).setWorkerPending('2026-09-23T12:00:00Z'));
		await page.getByRole('button', { name: 'Refresh' }).click();
		await expect(page.getByText('Update pending')).toBeVisible();
	});
});

test('worker mode polls metadata only — interval ticks never trigger a live refresh', async ({
	page
}) => {
	await page.clock.install();
	await page.goto('/tests/dialog/?mode=worker');
	await expect(page.getByText('Never deployed')).toBeVisible();
	const refreshes = (): Promise<boolean[]> =>
		page.evaluate(() =>
			(window as any).calls
				.filter((call: any) => call.cmd === 'get_server_sync_status')
				.map((call: any) => call.args.request.refresh)
		);
	const before = await refreshes();
	// Only mount-time live refreshes may hit the transport.
	expect(before.every((refresh) => refresh === true)).toBe(true);
	await page.clock.runFor(10_500);
	await expect.poll(async () => (await refreshes()).length).toBeGreaterThan(before.length);
	const after = await refreshes();
	expect(after.slice(before.length).every((refresh) => refresh === false)).toBe(true);
	expect(after.filter((refresh) => refresh === true).length).toBe(before.length);
});

for (const cancelStop of [false, true]) {
	test(`force-stop ${cancelStop ? 'honors' : 'confirms'} the cancel dialog`, async ({ page }) => {
		await page.goto(`/tests/dialog/?local=1&running=1${cancelStop ? '&cancelStop=1' : ''}`);
		const localTab = page.getByRole('tabpanel', { name: 'This computer' });
		await expect(localTab.getByRole('button', { name: 'Launch server' })).toBeDisabled();
		await localTab.getByRole('button', { name: 'Force stop server' }).click();
		const messageCall = () =>
			page.evaluate(() =>
				(window as any).calls.find((call: any) => call.cmd === 'plugin:dialog|message')
			);
		await expect.poll(messageCall).toBeTruthy();
		expect((await messageCall()).args.message).toContain('Unsaved world progress may be lost');
		if (cancelStop) {
			await expect(localTab.getByRole('button', { name: 'Launch server' })).toBeDisabled();
		} else {
			// The status card clears once the stopped status lands.
			await expect(localTab.getByRole('button', { name: 'Force stop server' })).toHaveCount(0);
			await expect(localTab.getByRole('button', { name: 'Launch server' })).toBeEnabled();
		}
		const stopped = await page.evaluate(() =>
			(window as any).calls.some((call: any) => call.cmd === 'force_stop_dedicated_server')
		);
		expect(stopped).toBe(!cancelStop);
	});
}

test('deployment actions fit the page at the default app size', async ({ page }) => {
	await page.setViewportSize({ width: 900, height: 700 });
	await page.goto('/tests/dialog/?many=1&status=upToDate');
	const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
	await remoteTab.getByText('Server config files').click();
	await remoteTab.getByRole('button', { name: 'Preview config changes' }).click();
	const row = page.getByTestId('server-config-row').filter({ hasText: 'file-003.cfg' }).first();
	await row.getByRole('button', { name: 'Apply' }).click();
	await expect(
		remoteTab.getByText('Selection changed. Preview again before deploying.')
	).toBeVisible();

	const scroll = page.getByTestId('server-page-scroll');
	const box = (await scroll.boundingBox())!;
	await expect.poll(() => scroll.evaluate((el) => el.scrollHeight)).toBeGreaterThan(700);
	const widths = await remoteTab
		.locator('button, [role="combobox"]')
		.evaluateAll((els) => els.map((el) => el.getBoundingClientRect().right));
	expect(Math.max(...widths)).toBeLessThanOrEqual(box.x + box.width + 1);
});

test('provisioning the local worker leaves no unsaved bar', async ({ page }) => {
	await page.goto('/tests/dialog/?unset=1');
	await page.getByRole('tab', { name: 'Remote server' }).click();
	const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
	await remoteTab.getByLabel('Host', { exact: true }).fill('example.test');
	await remoteTab.getByLabel('Username').fill('test-user');
	await remoteTab.getByLabel('Sync mode').click();
	await page.getByRole('option', { name: 'Worker on this PC' }).click();
	await remoteTab.getByRole('button', { name: 'Set up worker' }).click();

	// Provision persists hosted-worker mode itself, so the saved baseline
	// is rebased — nothing remains unsaved, and the service shows running.
	await expect(page.getByText('Unsaved changes')).toHaveCount(0);
	await expect(remoteTab.getByText('running', { exact: true })).toBeVisible();
	const provisioned = await page.evaluate(() =>
		(window as any).calls.some((call: any) => call.cmd === 'provision_local_worker')
	);
	expect(provisioned).toBe(true);
});

test('a failed live refresh reports unavailable instead of spinning forever', async ({ page }) => {
	await page.goto('/tests/dialog/?status=upToDate&failStatus=1');
	const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
	await expect(remoteTab.getByText("Couldn't check the server")).toBeVisible();
	await expect(remoteTab.getByText('Checking server')).toHaveCount(0);
	await expect(remoteTab.locator('.animate-spin')).toHaveCount(0);
});

test('an unpublished profile explains the blocker and disables deployment', async ({ page }) => {
	await page.goto('/tests/dialog/?nopub=1');
	const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
	await expect(
		remoteTab.getByText(
			"This profile hasn't been published yet. Publish it from the toolbar before deploying."
		)
	).toBeVisible();
	await expect(remoteTab.getByRole('button', { name: 'Preview' })).toBeDisabled();
	await remoteTab.getByText('Server config files').click();
	await expect(remoteTab.getByRole('button', { name: 'Preview config changes' })).toBeDisabled();
});

test('switching profiles resets the remote status instead of leaking it', async ({ page }) => {
	await page.goto('/tests/dialog/?status=upToDate');
	const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
	await expect(remoteTab.getByText('Up to date')).toBeVisible();

	await page.evaluate(() => (window as any).switchProfile(2));
	await expect(remoteTab.getByText('Never deployed')).toBeVisible();
	await expect(remoteTab.getByText('Up to date')).toHaveCount(0);
	// The new profile was queried fresh rather than reusing cached state.
	const afterSwitch = await page.evaluate(
		() => (window as any).calls.filter((call: any) => call.cmd === 'get_server_sync_status').length
	);
	expect(afterSwitch).toBeGreaterThan(0);
});

test('launch menu only offers vanilla and modded', async ({ page }) => {
	await page.goto('/tests/dialog/?component=launch');
	await page.getByRole('button').last().click();
	const menu = page.getByRole('menu');
	await expect(menu.getByText('Launch vanilla')).toBeVisible();
	await expect(menu.getByText('Launch modded')).toBeVisible();
	await expect(menu.getByText('server', { exact: false })).toHaveCount(0);
});
