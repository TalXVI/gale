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

	await page.goto(`/tests/dialog/?mode=local&uploads=2&unchanged=164`);
	await page
		.getByRole('tabpanel', { name: 'Remote server' })
		.getByRole('button', { name: 'Preview', exact: true })
		.click();
	await expect(page.getByText('No mod file changes found')).toHaveCount(0);
	await expect(page.getByText('Review the changes, then deploy.')).toBeVisible();
});

test('decisions do not reorder or scroll a long review list', async ({ page }) => {
	await page.setViewportSize({ width: 900, height: 650 });
	await page.goto(`/tests/dialog/?mode=local&many=1`);
	const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
	await remoteTab.getByText('Server config files', { exact: true }).click();
	await remoteTab.getByRole('button', { name: 'Preview config changes', exact: true }).click();
	await remoteTab.getByRole('button', { name: 'Show files needing review' }).click();
	// Wait for the review font before measuring decision-induced scrolling.
	await page.evaluate(() => document.fonts.ready.then(() => undefined));
	const rows = page.getByTestId('server-config-row');
	const list = rows.first().locator('..');
	const pageScroll = page.getByTestId('server-page-scroll');
	const before = await rows.evaluateAll((elements) =>
		elements.map((row) => row.getAttribute('data-path'))
	);
	const row = rows.nth(20);
	const restoreRow = rows.filter({ hasText: 'file-087.cfg' });
	await pageScroll.evaluate((element) => {
		element.scrollTop = element.scrollHeight / 2;
	});
	await list.evaluate((element) => {
		element.scrollTop = 500;
	});
	const positions = async () => ({
		inner: await list.evaluate((element) => element.scrollTop),
		outer: await pageScroll.evaluate((element) => element.scrollTop)
	});
	const checkDecision = async (
		selectedRow: typeof row,
		action: string,
		focusedAction: string,
		remaining: number
	) => {
		await selectedRow.scrollIntoViewIfNeeded();
		const beforeScroll = await positions();
		expect(beforeScroll.inner).toBeGreaterThan(0);
		expect(beforeScroll.outer).toBeGreaterThan(0);
		await selectedRow.getByRole('button', { name: action }).click();
		await expect(page.getByText(`${remaining} files still need a decision`)).toBeVisible();
		await expect(selectedRow.getByRole('button', { name: focusedAction })).toBeFocused();
		expect(await positions()).toEqual(beforeScroll);
		expect(
			await rows.evaluateAll((elements) => elements.map((item) => item.getAttribute('data-path')))
		).toEqual(before);
	};
	await checkDecision(row, 'Apply', 'Undo', 30);
	await checkDecision(row, 'Undo', 'Apply', 31);
	await checkDecision(row, 'Decline', 'Undo', 30);
	await checkDecision(row, 'Undo', 'Decline', 31);
	await checkDecision(restoreRow, 'Restore', 'Undo', 30);
	await remoteTab.getByRole('button', { name: 'Preview config changes', exact: true }).click();
	await expect(remoteTab.getByRole('button', { name: 'Push configs', exact: true })).toBeEnabled();
	await checkDecision(restoreRow, 'Undo', 'Restore', 31);
	await expect(remoteTab.getByRole('button', { name: 'Push configs', exact: true })).toBeDisabled();
	await remoteTab.getByRole('button', { name: 'Show all config files' }).click();
	await expect(rows).toHaveCount(133);
	await remoteTab.getByRole('button', { name: 'Show files needing review' }).click();
	await expect(rows).toHaveCount(31);
	expect(
		await rows.evaluateAll((elements) => elements.map((item) => item.getAttribute('data-path')))
	).toEqual(before);
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

test('unresolved configs lead the list and review focus preserves approval inputs', async ({
	page
}) => {
	await page.goto(`/tests/dialog/?mode=local&many=1`);
	const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
	await remoteTab.getByRole('button', { name: 'Refresh', exact: true }).waitFor();
	await remoteTab.getByText('Server config files', { exact: true }).click();
	await remoteTab.getByRole('button', { name: 'Preview config changes', exact: true }).click();
	const rows = page.getByTestId('server-config-row');
	await expect(rows).toHaveCount(133);
	await expect(rows.first()).toHaveAttribute('data-path', 'BepInEx/config/file-003.cfg');
	await expect(page.getByText('31 files still need a decision')).toBeVisible();
	await remoteTab.getByRole('button', { name: 'Show files needing review' }).click();
	await expect(rows).toHaveCount(31);
	await rows.first().getByRole('button', { name: 'Apply' }).click();
	await expect(page.getByText('30 files still need a decision')).toBeVisible();
	await expect(remoteTab.getByRole('button', { name: 'Push configs', exact: true })).toBeDisabled();
	await remoteTab.getByRole('button', { name: 'Preview config changes', exact: true }).click();
	await expect(remoteTab.getByRole('button', { name: 'Push configs', exact: true })).toBeEnabled();
	await remoteTab.getByRole('button', { name: 'Show all config files' }).click();
	await expect(rows).toHaveCount(133);
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
	const errors: string[] = [];
	page.on('pageerror', (error) => errors.push(error.message));
	await page.goto(`/tests/dialog/?mode=local&many=1`);
	const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
	await remoteTab.getByText('Server config files', { exact: true }).click();
	await remoteTab.getByRole('button', { name: 'Preview config changes', exact: true }).click();
	const rows = page.getByTestId('server-config-row');
	await expect(rows).toHaveCount(133);
	const order = await rows.evaluateAll((elements) =>
		elements.map((row) => row.getAttribute('data-path'))
	);
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
	await expect(page.getByText('31 files still need a decision')).toBeVisible();
	await expect(remoteTab.getByRole('button', { name: 'Push configs', exact: true })).toBeDisabled();
	expect(
		await rows.evaluateAll((elements) => elements.map((row) => row.getAttribute('data-path')))
	).toEqual(order);
	// Filtering the row away and back must not resurrect the old policy.
	await remoteTab.getByRole('button', { name: 'Show files needing review' }).click();
	await expect(rows).toHaveCount(31);
	await remoteTab.getByRole('button', { name: 'Show all config files' }).click();
	await expect(rows).toHaveCount(133);
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
	expect(errors).toEqual([]);
});

for (const mode of ['local', 'worker']) {
	for (const operation of [
		'preview_server_sync',
		'deploy_server_sync',
		'set_server_config_policy'
	]) {
		test(`${mode}: ${operation} locks edits and overlapping submissions`, async ({ page }) => {
			const errors: string[] = [];
			page.on('pageerror', (error) => errors.push(error.message));
			await page.goto(`/tests/dialog/?mode=${mode}`);
			const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
			const button = (name: string) => {
				if (name === 'Never')
					return remoteTab.getByLabel('Restart after this deploy', { exact: true });
				if (['Ask each update', 'Always apply updates'].includes(name))
					return page.getByLabel('Future updates for BepInEx/config/test.cfg', { exact: true });
				if (['Restore', 'Undo'].includes(name))
					return remoteTab.getByRole('button', {
						name: `${name} BepInEx/config/test.cfg`,
						exact: true
					});
				return remoteTab.getByRole('button', { name, exact: true });
			};
			await button('Refresh').waitFor();
			// A typed-but-unsaved credential rides along with the sync calls.
			const credential = remoteTab.getByLabel(mode === 'worker' ? 'Worker token' : 'Password', {
				exact: true
			});
			await credential.fill('test-secret');
			await remoteTab.getByText('Server config files', { exact: true }).click();
			await button('Preview config changes').click();
			await button('Restore').waitFor();
			await button('Restore').click();
			await expect(button('Push configs')).toBeDisabled();
			await button('Preview config changes').click();
			await expect(button('Push configs')).toBeEnabled();
			await page.evaluate((cmd) => (window as any).hold(cmd), operation);
			if (operation === 'set_server_config_policy') {
				await button('Ask each update').click();
				await page.getByRole('option', { name: 'Always apply updates', exact: true }).click();
			} else
				await button(
					operation === 'preview_server_sync' ? 'Preview config changes' : 'Push configs'
				).click();
			const configPreview = button('Preview config changes');
			const restart = button('Never');
			for (const control of [
				configPreview,
				restart,
				credential,
				button('Preview'),
				button('Push configs'),
				button('Refresh'),
				button('Undo'),
				button(
					operation === 'set_server_config_policy' ? 'Always apply updates' : 'Ask each update'
				)
			])
				await expect(control).toBeDisabled();
			if (mode === 'worker')
				for (const checkbox of await remoteTab.getByRole('checkbox').all())
					await expect(checkbox).toBeDisabled();
			const before = await page.evaluate(() => structuredClone((window as any).calls));
			// Physical pointer and keyboard attempts must not open selectors or submit again.
			for (const control of [configPreview, restart, button('Preview'), button('Push configs')]) {
				const box = (await control.boundingBox())!;
				await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
			}
			await page.keyboard.press('Escape');
			await expect(button('Preview')).toBeVisible();
			await expect(page.getByRole('option')).toHaveCount(0);
			await expect(credential).toHaveValue('test-secret');
			expect(await page.evaluate(() => (window as any).calls)).toEqual(before);
			const request = before.filter((call: any) => call.cmd === operation).at(-1).args.request;
			expect(request.restartPolicy ?? 'manual').toBe('manual');
			expect(request[mode === 'worker' ? 'workerToken' : 'password']).toBe('test-secret');
			if (request.selection)
				expect(request.selection).toEqual({
					includeMods: false,
					includeConfigs: true,
					applyConfigs: ['BepInEx/config/test.cfg'],
					restoreConfigs: ['BepInEx/config/test.cfg'],
					declineConfigs: []
				});
			if (operation === 'set_server_config_policy')
				expect(request).toMatchObject({ path: 'BepInEx/config/test.cfg', policy: 'alwaysApply' });
			if (operation === 'deploy_server_sync')
				expect(request).toMatchObject({ planHash: 'approved-plan', force: false });
			await page.evaluate(() => (window as any).release());
			await expect(configPreview).toBeEnabled();
			await expect(credential).toBeEnabled();
			if (operation === 'set_server_config_policy')
				await expect(button('Push configs')).toBeDisabled();
			if (mode === 'worker') {
				for (const checkbox of await remoteTab.getByRole('checkbox').all())
					await expect(checkbox).toBeEnabled();
			}
			await restart.click();
			await page.getByRole('option', { name: 'Immediately', exact: true }).click();
			await expect(button('Push configs')).toBeDisabled();
			await credential.fill('next-secret');
			// A mods preview is the primary flow again; configs stay out of it.
			await button('Preview').click();
			await expect(button('Deploy')).toBeEnabled();
			const next = await page.evaluate(
				() =>
					(window as any).calls.filter((call: any) => call.cmd === 'preview_server_sync').at(-1)
						.args.request
			);
			expect(next[mode === 'worker' ? 'workerToken' : 'password']).toBe('next-secret');
			expect(next.selection).toMatchObject({ includeMods: true, includeConfigs: false });
			expect(await page.evaluate(() => (window as any).unexpected)).toEqual([]);
		});
	}
}
