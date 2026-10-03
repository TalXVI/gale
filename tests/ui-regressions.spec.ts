import { expect, test } from '@playwright/test';

for (const staleMode of ['server', 'unexpected']) {
	test(`normalizes persisted launch mode ${staleMode}`, async ({ page }) => {
		await page.goto(`/tests/dialog/?component=regression&launchMode=${staleMode}`);
		await expect(page.getByRole('button', { name: 'Launch modded' })).toBeVisible();
		await expect
			.poll(async () =>
				page.evaluate(
					() =>
						(window as any).calls?.filter((call: any) => call.cmd === 'plugin:store|entries')
							.length ?? 0
				)
			)
			.toBe(1);
		await page.evaluate(() => (window as any).release());
		await page.waitForTimeout(100);
		const launch = page.getByRole('button', { name: 'Launch modded' });
		await expect(launch).toBeVisible();
		await launch.click();
		await expect
			.poll(async () =>
				page.evaluate(() => (window as any).calls.find((call: any) => call.cmd === 'launch_game'))
			)
			.toMatchObject({ args: { vanilla: false } });
	});
}

test('the launch menu offers only Vanilla and Modded, and the selected mode controls the main button', async ({
	page
}) => {
	await page.goto('/tests/dialog/?component=regression');
	await expect(page.getByRole('button', { name: 'Launch modded' })).toBeVisible();
	await page.locator('button').nth(1).click();
	await expect(page.getByRole('menuitem')).toHaveCount(2);
	await expect(page.getByRole('menuitem', { name: 'Launch vanilla' })).toBeVisible();
	await expect(page.getByRole('menuitem', { name: 'Launch modded' })).toBeVisible();
	await page.getByRole('menuitem', { name: 'Launch vanilla' }).click();
	await expect(page.getByRole('button', { name: 'Launch vanilla' })).toBeVisible();
	await expect
		.poll(async () =>
			page.evaluate(() => (window as any).calls.filter((call: any) => call.cmd === 'launch_game'))
		)
		.toMatchObject([{ args: { vanilla: true } }]);
	await page.getByRole('button', { name: 'Close dialog' }).click();
	await page.getByRole('button', { name: 'Launch vanilla' }).click();
	await expect
		.poll(async () =>
			page.evaluate(() => (window as any).calls.filter((call: any) => call.cmd === 'launch_game'))
		)
		.toMatchObject([{ args: { vanilla: true } }, { args: { vanilla: true } }]);
});

test('Escape cannot close a blocked dialog', async ({ page }) => {
	await page.goto('/tests/dialog/?component=regression&dialog=blocked');
	await expect(page.getByText('Dialog body')).toBeVisible();
	await page.waitForTimeout(100);
	await page.keyboard.press('Escape');
	await expect(page.getByText('Dialog body')).toBeVisible();
	await expect(page.getByTestId('open-state')).toHaveText('open');
	await expect(page.getByTestId('close-count')).toHaveText('0');
});

test('Escape honors confirmation and closes once', async ({ page }) => {
	await page.goto('/tests/dialog/?component=regression&dialog=confirm');
	await expect(page.getByText('Dialog body')).toBeVisible();
	await page.waitForTimeout(100);
	await page.keyboard.press('Escape');
	await expect(page.getByTestId('open-state')).toHaveText('open');
	await expect(page.getByTestId('close-count')).toHaveText('0');
	await expect
		.poll(async () =>
			page.evaluate(
				() =>
					(window as any).calls.filter(
						(call: any) => call.cmd === 'plugin:dialog|message' && call.args.buttons === 'OkCancel'
					).length
			)
		)
		.toBe(1);

	await page.goto('/tests/dialog/?component=regression&dialog=confirm&confirmClose=1');
	await expect(page.getByText('Dialog body')).toBeVisible();
	await page.waitForTimeout(100);
	await page.keyboard.press('Escape');
	await expect(page.getByTestId('open-state')).toHaveText('closed');
	await expect(page.getByTestId('close-count')).toHaveText('1');
});

test('outside interaction and accessible close button close once', async ({ page }) => {
	await page.goto('/tests/dialog/?component=regression&dialog=normal');
	await expect(page.getByText('Dialog body')).toBeVisible();
	await page.waitForTimeout(100);
	await page.mouse.click(5, 5);
	await expect(page.getByTestId('open-state')).toHaveText('closed');
	await expect(page.getByTestId('close-count')).toHaveText('1');
	await page.getByRole('button', { name: 'Reopen dialog' }).click();
	await page.getByRole('button', { name: 'Close dialog' }).click();
	await expect(page.getByTestId('close-count')).toHaveText('2');
});

test('outside interaction cannot close a blocked dialog', async ({ page }) => {
	await page.goto('/tests/dialog/?component=regression&dialog=blocked');
	await expect(page.getByText('Dialog body')).toBeVisible();
	await page.waitForTimeout(100);
	await page.mouse.click(5, 5);
	await expect(page.getByTestId('open-state')).toHaveText('open');
	await expect(page.getByTestId('close-count')).toHaveText('0');
});

test('outside interaction cannot bypass close confirmation', async ({ page }) => {
	await page.goto('/tests/dialog/?component=regression&dialog=confirm');
	await expect(page.getByText('Dialog body')).toBeVisible();
	await page.waitForTimeout(100);
	await page.mouse.click(5, 5);
	await expect(page.getByTestId('open-state')).toHaveText('open');
	await expect(page.getByTestId('close-count')).toHaveText('0');
});

test('a revoked sync session clears the displayed user', async ({ page }) => {
	await page.goto('/tests/dialog/?component=regression&authUser=1');
	await expect(page.getByTestId('sync-user')).toHaveText('Owner');
	await expect.poll(async () => page.evaluate(() => (window as any).authListenerCount())).toBe(1);
	await page.evaluate(() => (window as any).expireAuth());
	await expect(page.getByTestId('sync-user')).toHaveText('Sign in');
});
