<script lang="ts">
	import InputField from '$lib/components/ui/InputField.svelte';
	import Checkbox from '$lib/components/ui/Checkbox.svelte';
	import Label from '$lib/components/ui/Label.svelte';
	import Button from '$lib/components/ui/Button.svelte';
	import Info from '$lib/components/ui/Info.svelte';
	import InfoBox from '$lib/components/ui/InfoBox.svelte';
	import SecretField from './SecretField.svelte';
	import server from '$lib/state/server.svelte';
	import * as api from '$lib/api';
	import { confirm } from '@tauri-apps/plugin-dialog';
	import { m } from '$lib/paraglide/messages';
	import type { ServerFormState } from './serverForm.svelte';

	let { form }: { form: ServerFormState } = $props();
	const formId = $props.id();

	let stopping = $state(false);

	async function stop() {
		if (!(await confirm(m.dedicatedServerDialog_forceStopConfirm(), { kind: 'warning' }))) return;
		stopping = true;
		try {
			await api.profile.server.forceStop();
			await server.refresh();
		} finally {
			stopping = false;
		}
	}
</script>

{#if server.status.state === 'running'}
	<div
		class="border-primary-300 dark:border-primary-600 bg-primary-50 dark:bg-primary-900 mt-4 flex flex-wrap items-center gap-3 rounded-lg border p-3"
		role="status"
	>
		<span
			class="inline-flex items-center gap-1.5 rounded-full bg-green-100 px-2.5 py-0.5 text-sm font-medium text-green-700 dark:bg-green-900 dark:text-green-300"
		>
			<span class="size-2 rounded-full bg-green-500"></span>
			{server.status.stopping ? m.dedicatedServerDialog_stopping() : m.serverPage_statusRunning()}
		</span>
		<Button class="ml-auto" color="red" loading={stopping || server.status.stopping} onclick={stop}>
			{m.dedicatedServerDialog_forceStop()}
		</Button>
	</div>
	<InfoBox type="info" class="mt-2">{m.dedicatedServerDialog_running()}</InfoBox>
{/if}

<div class="mt-4 flex flex-col gap-3">
	<div>
		<Label for={`${formId}-field-1`}>{m.dedicatedServerDialog_serverName()}</Label><InputField
			id={`${formId}-field-1`}
			class="mt-1 w-full"
			bind:value={form.form.serverName}
			placeholder={m.dedicatedServerDialog_serverNamePlaceholder()}
		/>
	</div>
	<div>
		<Label for={`${formId}-field-2`}>{m.dedicatedServerDialog_world()}</Label><InputField
			id={`${formId}-field-2`}
			class="mt-1 w-full"
			bind:value={form.form.world}
			placeholder={m.dedicatedServerDialog_worldPlaceholder()}
		/>
	</div>
	<div>
		<SecretField
			id={`${formId}-field-3`}
			label={m.dedicatedServerDialog_password()}
			bind:value={form.gamePassword}
			saved={form.savedCredentials?.gamePassword ?? false}
			rememberLabel={m.dedicatedServerDialog_rememberPassword()}
			bind:remember={form.rememberGamePassword}
		/>
		{#if !form.rememberGamePassword}
			<p id={`${formId}-password-help`} class="text-primary-500 mt-1 text-sm">
				{m.dedicatedServerDialog_sessionPassword()}
			</p>
		{/if}
	</div>
	<div>
		<Label for={`${formId}-field-5`}>{m.dedicatedServerDialog_serverPort()}</Label><InputField
			id={`${formId}-field-5`}
			class="mt-1 w-full"
			bind:value={form.port}
			inputmode="numeric"
		/>
	</div>
	<div class="flex items-center">
		<Label for={`${formId}-field-6`}>{m.dedicatedServerDialog_public()}</Label><Info
			>{m.dedicatedServerDialog_publicInfo()}</Info
		><Checkbox id={`${formId}-field-6`} bind:checked={form.form.publicServer} />
	</div>
	<div class="flex items-center">
		<Label for={`${formId}-field-7`}>{m.dedicatedServerDialog_crossplay()}</Label><Info
			>{m.dedicatedServerDialog_crossplayInfo()}</Info
		><Checkbox id={`${formId}-field-7`} bind:checked={form.form.crossplay} />
	</div>

	<details class="mt-2">
		<summary class="text-primary-600 dark:text-primary-300 cursor-pointer"
			>{m.dedicatedServerDialog_advancedOptions()}</summary
		>
		<div class="mt-2">
			<Label for={`${formId}-field-24`}>{m.dedicatedServerDialog_additionalArgs()}</Label
			><InputField
				id={`${formId}-field-24`}
				class="mt-1 w-full"
				bind:value={form.form.extraArgs}
				placeholder="-savedir ..."
			/>
		</div>
	</details>

	<div class="mt-2">
		<Button
			icon="mdi:server"
			disabled={form.busy || server.status.state === 'running'}
			loading={form.launching}
			onclick={() => form.launch()}>{m.dedicatedServerDialog_launch()}</Button
		>
	</div>
</div>
