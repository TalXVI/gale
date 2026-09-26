import { test, expect } from '@playwright/test';

test('worker poll and deployment errors render independently', async ({ page }) => {
	await page.goto('/tests/dialog/?mode=worker');
	const pollError = 'Publication check failed: sync token request failed';
	const deploymentError = 'automatic deployment failed: upload failed';
	await page.evaluate(
		([poll, deployment]) => (window as any).setWorkerErrors(poll, deployment),
		[pollError, deploymentError]
	);
	await page.getByRole('button', { name: 'Refresh', exact: true }).click();
	await expect(page.getByText(pollError)).toBeVisible();
	await expect(page.getByText(deploymentError)).toBeVisible();

	await page.evaluate(
		(deployment) => (window as any).setWorkerErrors(null, deployment),
		deploymentError
	);
	await page.getByRole('button', { name: 'Refresh', exact: true }).click();
	await expect(page.getByText(pollError)).toHaveCount(0);
	await expect(page.getByText(deploymentError)).toBeVisible();
});

test('a pending publication is owed mods; routine status never reports config work', async ({
	page
}) => {
	await page.goto('/tests/dialog/?mode=worker&restart');
	await page.evaluate(() => (window as any).setWorkerPending('2026-09-23T12:00:00Z'));
	await page.getByRole('button', { name: 'Refresh', exact: true }).click();
	await expect(page.getByText('Update pending')).toBeVisible();
	await expect(
		page.getByText(
			'A publication is pending. Deploy it manually, or enable automatic synchronization.'
		)
	).toBeVisible();
	// Config divergence is server-owned state, never routine pending
	// work: even a populated status summary carries no config lines.
	await expect(page.getByText(/config file\(s\) awaiting a decision/)).toHaveCount(0);
	await expect(page.getByText(/config sync/i)).toHaveCount(0);
	await page.evaluate(() => (window as any).setWorkerPending(null));
	await page.getByRole('button', { name: 'Refresh', exact: true }).click();
	await expect(page.getByText('Update pending')).toHaveCount(0);
});

for (const mode of ['local', 'worker']) {
	test(`${mode}: the primary sync flow deploys mods only`, async ({ page }) => {
		await page.goto(`/tests/dialog/?mode=${mode}`);
		const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
		await remoteTab.getByRole('button', { name: 'Preview', exact: true }).click();
		await expect(remoteTab.getByRole('button', { name: 'Deploy', exact: true })).toBeEnabled();
		await expect(page.getByTestId('server-config-row')).toHaveCount(0);
		const modsSelection = await page.evaluate(
			() =>
				(window as any).calls.filter((call: any) => call.cmd === 'preview_server_sync').at(-1).args
					.request.selection
		);
		expect(modsSelection).toEqual({
			includeMods: true,
			includeConfigs: false,
			applyConfigs: [],
			restoreConfigs: [],
			declineConfigs: []
		});
		await remoteTab.getByRole('button', { name: 'Deploy', exact: true }).click();
		await expect(page.getByText('Mod deployment finished.')).toBeVisible();
		// Config deployment stays available as a separate explicit action.
		await remoteTab.getByText('Server config files', { exact: true }).click();
		await remoteTab.getByRole('button', { name: 'Preview config changes', exact: true }).click();
		await expect(page.getByTestId('server-config-row')).toHaveCount(1);
		const configSelection = await page.evaluate(
			() =>
				(window as any).calls.filter((call: any) => call.cmd === 'preview_server_sync').at(-1).args
					.request.selection
		);
		expect(configSelection).toMatchObject({ includeMods: false, includeConfigs: true });
		await expect(
			remoteTab.getByRole('button', { name: 'Push configs', exact: true })
		).toBeEnabled();
	});
}

// The config-review behaviors below are identical in both sync modes;
// they run once against the local path.
test('a zero-change preview says so explicitly and keeps Deploy', async ({ page }) => {
	await page.goto(`/tests/dialog/?mode=local&unchanged=166&unmanaged=2`);
	const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
	await remoteTab.getByRole('button', { name: 'Preview', exact: true }).click();
	await expect(
		page.getByText('No mod file changes found. The server already matches the published mod files.')
	).toBeVisible();
	// The generic review instruction must not imply changes to inspect.
	await expect(page.getByText('Review the changes, then deploy.')).toHaveCount(0);
	await expect(
		page.getByText('Deploying records this publication as deployed without changing any files.')
	).toBeVisible();
	// Unmanaged files are reported separately; they are not changes.
	await expect(
		page.getByText('2 unmanaged file(s) on the server were left untouched.')
	).toBeVisible();
	// Deploy stays available: a zero-diff deploy still records the
	// publication's mod revision as deployed on the server.
	await expect(remoteTab.getByRole('button', { name: 'Deploy', exact: true })).toBeEnabled();
});

test('restart preference survives reload and stays profile-specific', async ({ page }) => {
	await page.goto(`/tests/dialog/?mode=local&profile=first`);
	const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
	await remoteTab.getByLabel('Restart after this deploy', { exact: true }).click();
	await page.getByRole('option', { name: 'When empty', exact: true }).click();
	await page.reload();
	await expect(remoteTab.getByLabel('Restart after this deploy', { exact: true })).toHaveText(
		'When empty'
	);
	await page.goto(`/tests/dialog/?mode=local&profile=second`);
	await expect(
		page.getByRole('tabpanel', { name: 'Remote server' }).getByLabel('Restart after this deploy', {
			exact: true
		})
	).toHaveText('Never');
	await page.goto(`/tests/dialog/?mode=local&profile=first`);
	await expect(
		page.getByRole('tabpanel', { name: 'Remote server' }).getByLabel('Restart after this deploy', {
			exact: true
		})
	).toHaveText('When empty');
});

test('config review retains decisions across filtering and invalidates approval', async ({
	page
}) => {
	await page.goto(`/tests/dialog/?mode=local&many=1`);
	const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
	await remoteTab.getByRole('button', { name: 'Refresh', exact: true }).waitFor();
	await remoteTab.getByText('Server config files', { exact: true }).click();
	await remoteTab.getByRole('button', { name: 'Preview config changes', exact: true }).click();
	const rows = page.getByTestId('server-config-row');
	await expect(rows).toHaveCount(12);
	await expect(rows.first()).toHaveAttribute('data-path', 'BepInEx/config/file-003.cfg');
	await expect(page.getByText('3 files still need a decision')).toBeVisible();
	await remoteTab.getByRole('button', { name: 'Show files needing review' }).click();
	await expect(rows).toHaveCount(3);
	await rows.first().getByRole('button', { name: 'Apply' }).click();
	await expect(page.getByText('2 files still need a decision')).toBeVisible();
	await expect(remoteTab.getByRole('button', { name: 'Push configs', exact: true })).toBeDisabled();
	await remoteTab.getByRole('button', { name: 'Preview config changes', exact: true }).click();
	await expect(remoteTab.getByRole('button', { name: 'Push configs', exact: true })).toBeEnabled();
	await remoteTab.getByRole('button', { name: 'Show all config files' }).click();
	await expect(rows).toHaveCount(12);
	await expect(remoteTab.getByRole('button', { name: 'Push configs', exact: true })).toBeEnabled();
	await expect(rows.filter({ hasText: 'file-000.cfg' })).toBeVisible();
	const selected = await page.evaluate(
		() =>
			(window as any).calls.filter((call: any) => call.cmd === 'preview_server_sync').at(-1).args
				.request.selection
	);
	expect(selected).toMatchObject({
		includeMods: false,
		includeConfigs: true,
		applyConfigs: ['BepInEx/config/file-003.cfg']
	});
});

test('explicit restart confirmation clears the reminder', async ({ page }) => {
	await page.goto(`/tests/dialog/?mode=local&restart=1`);
	const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
	await remoteTab.getByRole('button', { name: 'Preview', exact: true }).click();
	await expect(remoteTab.getByRole('button', { name: 'Deploy', exact: true })).toBeEnabled();
	await remoteTab.getByRole('button', { name: 'I confirmed the restart' }).click();
	await expect(remoteTab.getByRole('button', { name: 'I confirmed the restart' })).toHaveCount(0);
	await expect(remoteTab.getByRole('button', { name: 'Deploy', exact: true })).toBeDisabled();
	const calls = await page.evaluate(() => (window as any).calls);
	expect(calls.some((call: any) => call.cmd === 'plugin:dialog|message')).toBe(true);
	expect(calls.some((call: any) => call.cmd === 'acknowledge_external_server_restart')).toBe(true);
});

test('saved future policies stick, failed saves revert', async ({ page }) => {
	await page.goto(`/tests/dialog/?mode=local&many=1`);
	const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
	await remoteTab.getByText('Server config files', { exact: true }).click();
	await remoteTab.getByRole('button', { name: 'Preview config changes', exact: true }).click();
	const rows = page.getByTestId('server-config-row');
	await expect(rows).toHaveCount(12);
	const policy = page.getByLabel('Future updates for BepInEx/config/file-000.cfg', {
		exact: true
	});
	await expect(policy).toHaveText('Ask each update');
	await policy.click();
	await page.getByRole('option', { name: 'Always apply updates', exact: true }).click();
	// The command returns nothing; the saved value is reflected locally.
	await expect(policy).toHaveText('Always apply updates');
	// A future policy is not a current Apply/Decline decision — but it
	// does invalidate the approved deployment inputs.
	await expect(page.getByText('3 files still need a decision')).toBeVisible();
	await expect(remoteTab.getByRole('button', { name: 'Push configs', exact: true })).toBeDisabled();
	// Filtering the row away and back must not resurrect the old policy.
	await remoteTab.getByRole('button', { name: 'Show files needing review' }).click();
	await expect(rows).toHaveCount(3);
	await remoteTab.getByRole('button', { name: 'Show all config files' }).click();
	await expect(rows).toHaveCount(12);
	await expect(policy).toHaveText('Always apply updates');
	// A failed write snaps back to the last saved policy and reports it.
	await page.evaluate(() => (window as any).fail('set_server_config_policy'));
	await policy.click();
	await page.getByRole('option', { name: 'Always keep my config', exact: true }).click();
	await expect(policy).toHaveText('Always apply updates');
	expect(
		await page.evaluate(
			() =>
				(window as any).calls.filter(
					(call: any) => call.cmd === 'plugin:dialog|message' && call.args.kind === 'error'
				).length
		)
	).toBe(1);
});

for (const operation of ['preview_server_sync', 'deploy_server_sync', 'set_server_config_policy']) {
	test(`${operation} blocks another config action until it settles`, async ({ page }) => {
		await page.goto('/tests/dialog/?mode=local');
		const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
		await remoteTab.getByText('Server config files', { exact: true }).click();
		const preview = remoteTab.getByRole('button', { name: 'Preview config changes' });
		if (operation !== 'preview_server_sync') await preview.click();
		await page.evaluate((command) => (window as any).hold(command), operation);

		if (operation === 'preview_server_sync') {
			await preview.click();
		} else if (operation === 'deploy_server_sync') {
			await remoteTab.getByRole('button', { name: 'Restore BepInEx/config/test.cfg' }).click();
			await preview.click();
			await remoteTab.getByRole('button', { name: 'Push configs' }).click();
		} else {
			await page.getByLabel('Future updates for BepInEx/config/test.cfg').click();
			await page.getByRole('option', { name: 'Always apply updates' }).click();
		}

		await expect
			.poll(() =>
				page.evaluate(
					(command) => (window as any).calls.filter((call: any) => call.cmd === command).length,
					operation
				)
			)
			.toBe(1);
		await expect(preview).toBeDisabled();
		await expect(remoteTab.getByRole('button', { name: 'Refresh', exact: true })).toBeDisabled();
		await expect(remoteTab.getByLabel('Restart after this deploy')).toBeDisabled();
		await page.evaluate(() => (window as any).release());
		await expect(preview).toBeEnabled();
	});
}
