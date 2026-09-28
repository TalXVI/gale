import { test, expect, type Page } from '@playwright/test';

for (const [scenario, expected] of [
	['', 'Deleted from server'],
	['&many=1', 'Modified on server']
] as const) {
	test(`server config review describes the remote copy: ${expected}`, async ({ page }) => {
		await page.goto(`/tests/dialog/?status=upToDate${scenario}`);
		const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
		await remoteTab.getByText('Server config files', { exact: true }).click();
		await remoteTab.getByRole('button', { name: 'Preview config changes' }).click();
		await expect(page.getByTestId('server-config-row').first().getByText(expected)).toBeVisible();
	});
}

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
			'A publication is pending. Deploy its mod updates manually, or enable automatic mod deployment.'
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
	// A future policy is not a current Apply/Decline decision, but it
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

// `?mixed=1` seeds 16 config entries: 8 pending (5 modified on the
// server, 3 deleted from it) plus markApplied/write/unapplied/decline
// rows the bulk buttons must leave alone.
const mixedPending = Array.from(
	{ length: 8 },
	(_, index) => `BepInEx/config/mixed-${String(index).padStart(2, '0')}.cfg`
);
const mixedDeleted = mixedPending.slice(5);

function lastSelection(page: Page, cmd: string) {
	return page.evaluate(
		(command) =>
			(window as any).calls.filter((call: any) => call.cmd === command).at(-1).args.request
				.selection,
		cmd
	);
}

test('bulk decisions stage every pending config without a server call', async ({ page }) => {
	await page.goto('/tests/dialog/?mode=local&mixed=1');
	const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
	await remoteTab.getByText('Server config files', { exact: true }).click();
	const preview = remoteTab.getByRole('button', { name: 'Preview config changes', exact: true });
	await preview.click();
	const rows = page.getByTestId('server-config-row');
	await expect(rows).toHaveCount(16);
	await expect(rows.filter({ hasText: 'Modified on server' })).toHaveCount(5);
	await expect(rows.filter({ hasText: 'Deleted from server' })).toHaveCount(3);
	await expect(page.getByText('8 files still need a decision')).toBeVisible();

	// Per-file picks the bulk buttons must overwrite; the 'unapplied'
	// row keeps its pick because bulk decisions only touch pending.
	await remoteTab
		.getByRole('button', { name: 'Decline BepInEx/config/mixed-00.cfg', exact: true })
		.click();
	await remoteTab
		.getByRole('button', { name: 'Restore BepInEx/config/mixed-05.cfg', exact: true })
		.click();
	await remoteTab
		.getByRole('button', { name: 'Apply BepInEx/config/mixed-11.cfg', exact: true })
		.click();
	await expect(page.getByText('6 files still need a decision')).toBeVisible();

	const callCounts = () =>
		page.evaluate(() => {
			const calls = (window as any).calls;
			const count = (cmd: string) => calls.filter((call: any) => call.cmd === cmd).length;
			return {
				preview: count('preview_server_sync'),
				deploy: count('deploy_server_sync'),
				policy: count('set_server_config_policy')
			};
		});

	const beforeApply = await callCounts();
	await remoteTab.getByRole('button', { name: 'Apply all', exact: true }).click();
	expect(await callCounts()).toEqual(beforeApply);
	await expect(page.getByText('0 files still need a decision')).toBeVisible();
	// The declined pending row was overwritten; the unapplied row's
	// individual Apply stands.
	await expect(rows.filter({ hasText: 'Will apply' })).toHaveCount(9);
	await expect(rows.filter({ hasText: 'Will decline' })).toHaveCount(0);

	await preview.click();
	let selected = await lastSelection(page, 'preview_server_sync');
	expect([...selected.applyConfigs].sort()).toEqual(
		[...mixedPending, 'BepInEx/config/mixed-11.cfg'].sort()
	);
	expect([...selected.restoreConfigs].sort()).toEqual([...mixedDeleted].sort());
	expect(selected.declineConfigs).toEqual([]);

	const beforeDecline = await callCounts();
	await remoteTab.getByRole('button', { name: 'Decline all', exact: true }).click();
	expect(await callCounts()).toEqual(beforeDecline);
	await expect(page.getByText('0 files still need a decision')).toBeVisible();
	await expect(rows.filter({ hasText: 'Will decline' })).toHaveCount(8);
	await expect(rows.filter({ hasText: 'Will apply' })).toHaveCount(1);

	await preview.click();
	selected = await lastSelection(page, 'preview_server_sync');
	expect([...selected.declineConfigs].sort()).toEqual([...mixedPending].sort());
	expect(selected.applyConfigs).toEqual(['BepInEx/config/mixed-11.cfg']);
	expect(selected.restoreConfigs).toEqual([]);

	await remoteTab.getByRole('button', { name: 'Push configs', exact: true }).click();
	await expect(page.getByText('Config synchronization finished.')).toBeVisible();
	const deployed = await lastSelection(page, 'deploy_server_sync');
	expect([...deployed.declineConfigs].sort()).toEqual([...mixedPending].sort());
	expect(deployed.applyConfigs).toEqual(['BepInEx/config/mixed-11.cfg']);
	expect(deployed.restoreConfigs).toEqual([]);
});

test('bulk decisions under the review filter still cover every pending entry', async ({ page }) => {
	await page.goto('/tests/dialog/?mode=local&mixed=1');
	const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
	await remoteTab.getByText('Server config files', { exact: true }).click();
	const preview = remoteTab.getByRole('button', { name: 'Preview config changes', exact: true });
	await preview.click();
	const rows = page.getByTestId('server-config-row');
	await expect(rows).toHaveCount(16);
	await remoteTab.getByRole('button', { name: 'Show files needing review', exact: true }).click();
	await expect(rows).toHaveCount(8);

	await remoteTab.getByRole('button', { name: 'Apply all', exact: true }).click();
	await expect(page.getByText('0 files still need a decision')).toBeVisible();
	await remoteTab.getByRole('button', { name: 'Show all config files', exact: true }).click();
	await expect(rows).toHaveCount(16);
	await preview.click();
	let selected = await lastSelection(page, 'preview_server_sync');
	expect([...selected.applyConfigs].sort()).toEqual([...mixedPending].sort());
	expect([...selected.restoreConfigs].sort()).toEqual([...mixedDeleted].sort());
	expect(selected.declineConfigs).toEqual([]);
	// Non-pending rows were never staged.
	const applied = page.locator('[data-path="BepInEx/config/mixed-08.cfg"]');
	await expect(applied).toContainText('Up to date');
	await expect(applied).not.toContainText('Will');
	const declined = page.locator('[data-path="BepInEx/config/mixed-13.cfg"]');
	await expect(declined).toContainText('Declined');
	await expect(declined).not.toContainText('Will');

	await remoteTab.getByRole('button', { name: 'Show files needing review', exact: true }).click();
	await expect(rows).toHaveCount(8);
	await remoteTab.getByRole('button', { name: 'Decline all', exact: true }).click();
	await remoteTab.getByRole('button', { name: 'Show all config files', exact: true }).click();
	await preview.click();
	selected = await lastSelection(page, 'preview_server_sync');
	expect([...selected.declineConfigs].sort()).toEqual([...mixedPending].sort());
	expect(selected.applyConfigs).toEqual([]);
	expect(selected.restoreConfigs).toEqual([]);
});

test('bulk decisions preserve scroll position and focus', async ({ page }) => {
	await page.setViewportSize({ width: 1400, height: 650 });
	await page.goto('/tests/dialog/?mode=local&mixed=1');
	const remoteTab = page.getByRole('tabpanel', { name: 'Remote server' });
	await remoteTab.getByText('Server config files', { exact: true }).click();
	await remoteTab.getByRole('button', { name: 'Preview config changes', exact: true }).click();
	const rows = page.getByTestId('server-config-row');
	await expect(rows).toHaveCount(16);
	const applyAll = remoteTab.getByRole('button', { name: 'Apply all', exact: true });
	const declineAll = remoteTab.getByRole('button', { name: 'Decline all', exact: true });

	const inner = rows.first().locator('xpath=..');
	expect(await inner.evaluate((el) => el.scrollHeight > el.clientHeight)).toBe(true);
	const outerHandle = await inner.evaluateHandle((el) => {
		let node = el.parentElement;
		while (node) {
			const { overflowY } = getComputedStyle(node);
			if ((overflowY === 'auto' || overflowY === 'scroll') && node.scrollHeight > node.clientHeight)
				return node;
			node = node.parentElement;
		}
		return null;
	});
	const outer = outerHandle.asElement();
	expect(outer).not.toBeNull();

	const innerTop = await inner.evaluate((el) => {
		el.scrollTop = Math.floor((el.scrollHeight - el.clientHeight) / 2);
		return el.scrollTop;
	});
	expect(innerTop).toBeGreaterThan(0);
	const outerTop = await outer!.evaluate(
		(el, anchor) => {
			el.scrollTop += anchor.getBoundingClientRect().top - window.innerHeight / 2;
			return el.scrollTop;
		},
		await applyAll.elementHandle()
	);
	expect(outerTop).toBeGreaterThan(0);
	const windowY = await page.evaluate(() => window.scrollY);
	const settle = () =>
		page.evaluate(
			() =>
				new Promise<void>((resolve) =>
					requestAnimationFrame(() => requestAnimationFrame(() => resolve()))
				)
		);

	await applyAll.click();
	await expect(page.getByText('0 files still need a decision')).toBeVisible();
	await settle();
	expect(await inner.evaluate((el) => el.scrollTop)).toBe(innerTop);
	expect(await outer!.evaluate((el) => el.scrollTop)).toBe(outerTop);
	expect(await page.evaluate(() => window.scrollY)).toBe(windowY);
	await expect(applyAll).toBeFocused();
	await expect(rows).toHaveCount(16);

	await declineAll.click();
	await expect(rows.filter({ hasText: 'Will decline' })).toHaveCount(8);
	await settle();
	expect(await inner.evaluate((el) => el.scrollTop)).toBe(innerTop);
	expect(await outer!.evaluate((el) => el.scrollTop)).toBe(outerTop);
	expect(await page.evaluate(() => window.scrollY)).toBe(windowY);
	await expect(declineAll).toBeFocused();
	await expect(rows).toHaveCount(16);
});
