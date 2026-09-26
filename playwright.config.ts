import { defineConfig } from '@playwright/test';

export default defineConfig({
	testDir: './tests',
	use: { baseURL: 'http://127.0.0.1:5174', viewport: { width: 1400, height: 1050 } },
	webServer: {
		command:
			'pnpm exec vite --config tests/vite.config.ts --host 127.0.0.1 --port 5174 --strictPort',
		url: 'http://127.0.0.1:5174/tests/dialog/',
		reuseExistingServer: false
	}
});
