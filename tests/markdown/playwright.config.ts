import { defineConfig } from '@playwright/test';

export default defineConfig({
	testDir: '..',
	testMatch: 'markdown-security.spec.ts'
});
