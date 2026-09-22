import { test, expect } from '@playwright/test';

for (const mode of ['local', 'worker']) {
	for (const operation of [
		'preview_server_sync',
		'deploy_server_sync',
		'set_server_config_policy',
		...(mode === 'worker' ? ['configure_worker'] : [])
	]) {
		test(`${mode}: ${operation} locks edits and overlapping submissions`, async ({ page }) => {
			const errors: string[] = [];
			page.on('pageerror', (error) => errors.push(error.message));
			await page.goto(`/tests/dialog/?mode=${mode}`);
			const button = (name: string) => page.getByRole('button', { name, exact: true });
			await button('Refresh').waitFor();
			await page.getByText('Credentials', { exact: true }).click();
			const password = page.locator('input[type=password]');
			await password.fill('test-secret');
			if (mode === 'worker') await page.getByText('Worker automation', { exact: true }).click();
			await button('Preview').click();
			await button('Restore').waitFor();
			await button('Restore').click();
			await expect(button('Deploy')).toBeDisabled();
			await button('Preview').click();
			await expect(button('Deploy')).toBeEnabled();
			if (operation === 'configure_worker') {
				await page.getByRole('checkbox').first().click();
				await page.getByRole('checkbox').last().click();
			}
			await page.evaluate((cmd) => (window as any).hold(cmd), operation);
			if (operation === 'set_server_config_policy') {
				await button('Ask each update').click();
				await page.getByRole('option', { name: 'Always apply updates', exact: true }).click();
			} else
				await button(
					operation === 'configure_worker'
						? 'Save automation'
						: operation === 'deploy_server_sync'
							? 'Deploy'
							: 'Preview'
				).click();
			const scope = button('Mods and selected configs');
			const restart = button('Never (manual)');
			for (const control of [
				scope,
				restart,
				password,
				button('Preview'),
				button('Deploy'),
				button('Close'),
				button('Refresh'),
				button('Undo'),
				button(
					operation === 'set_server_config_policy' ? 'Always apply updates' : 'Ask each update'
				)
			])
				await expect(control).toBeDisabled();
			if (mode === 'worker') {
				await expect(button('Save automation')).toBeDisabled();
				for (const checkbox of await page.getByRole('checkbox').all())
					await expect(checkbox).toBeDisabled();
			}
			const before = await page.evaluate(() => structuredClone((window as any).calls));
			// Physical pointer and keyboard attempts must not open selectors or submit again.
			for (const control of [scope, restart, button('Preview'), button('Deploy')]) {
				const box = (await control.boundingBox())!;
				await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
			}
			await page.keyboard.press('Escape');
			await expect(page.getByRole('dialog')).toBeVisible();
			await expect(page.getByRole('option')).toHaveCount(0);
			await expect(password).toHaveValue('test-secret');
			expect(await page.evaluate(() => (window as any).calls)).toEqual(before);
			const request = before.filter((call: any) => call.cmd === operation).at(-1).args.request;
			expect(request.restartPolicy ?? 'manual').toBe('manual');
			expect(request[mode === 'worker' ? 'workerToken' : 'password']).toBe('test-secret');
			if (request.selection)
				expect(request.selection).toEqual({
					includeMods: true,
					includeConfigs: true,
					applyConfigs: ['BepInEx/config/test.cfg'],
					restoreConfigs: ['BepInEx/config/test.cfg'],
					declineConfigs: []
				});
			if (operation === 'configure_worker')
				expect(request).toEqual({
					autoSync: true,
					autoMods: true,
					restartPolicy: 'manual',
					workerToken: 'test-secret'
				});
			if (operation === 'set_server_config_policy')
				expect(request).toMatchObject({ path: 'BepInEx/config/test.cfg', policy: 'alwaysApply' });
			if (operation === 'deploy_server_sync')
				expect(request).toMatchObject({ planHash: 'approved-plan', force: false });
			await page.evaluate(() => (window as any).release());
			await expect(scope).toBeEnabled();
			await expect(password).toBeEnabled();
			if (operation === 'set_server_config_policy') await expect(button('Deploy')).toBeDisabled();
			if (mode === 'worker') {
				for (const checkbox of await page.getByRole('checkbox').all())
					await expect(checkbox).toBeEnabled();
			}
			await restart.click();
			await page.getByRole('option', { name: 'Immediately', exact: true }).click();
			await expect(button('Deploy')).toBeDisabled();
			await scope.click();
			await page.getByRole('option', { name: 'Selected configs only', exact: true }).click();
			await password.fill('next-secret');
			await button('Preview').click();
			await expect(button('Deploy')).toBeEnabled();
			const next = await page.evaluate(
				() =>
					(window as any).calls.filter((call: any) => call.cmd === 'preview_server_sync').at(-1)
						.args.request
			);
			expect(next).toMatchObject({
				restartPolicy: 'immediate',
				selection: { includeMods: false, includeConfigs: true },
				[mode === 'worker' ? 'workerToken' : 'password']: 'next-secret'
			});
			expect(await page.evaluate(() => (window as any).unexpected)).toEqual([]);
			expect(errors).toEqual([]);
		});
	}
}
