<script lang="ts">
	import InputField from '$lib/components/ui/InputField.svelte';
	import Select from '$lib/components/ui/Select.svelte';
	import Label from '$lib/components/ui/Label.svelte';
	import Button from '$lib/components/ui/Button.svelte';
	import Info from '$lib/components/ui/Info.svelte';
	import PathField from '$lib/components/ui/PathField.svelte';
	import SecretField from './SecretField.svelte';
	import type { RemoteProtocol } from '$lib/types';
	import { m } from '$lib/paraglide/messages';
	import type { ServerFormState } from './serverForm.svelte';

	let { form }: { form: ServerFormState } = $props();
	const formId = $props.id();

	const protocolInfo = $derived(
		form.form.remote.protocol === 'sftp'
			? m.dedicatedServerDialog_sftpInfo()
			: form.form.remote.protocol === 'ftps'
				? m.dedicatedServerDialog_ftpsInfo()
				: m.dedicatedServerDialog_ftpInfo()
	);
</script>

<div class="mt-2 flex flex-col gap-3">
	<div>
		<div class="flex items-center">
			<Label inline for={`${formId}-protocol`}>{m.dedicatedServerDialog_protocol()}</Label>
			<Info>{protocolInfo}</Info>
		</div>
		<Select
			id={`${formId}-protocol`}
			type="single"
			triggerClass="mt-1 w-full"
			value={form.form.remote.protocol}
			onValueChange={(value) => form.changeRemoteProtocol(value as RemoteProtocol)}
			items={[
				{ value: 'sftp', label: m.dedicatedServerDialog_protocolSftp() },
				{ value: 'ftps', label: m.dedicatedServerDialog_protocolFtps() },
				{ value: 'ftp', label: m.dedicatedServerDialog_protocolFtp() }
			]}
		/>
		{#if form.form.remote.protocol === 'ftp'}
			<p class="mt-1 text-sm text-orange-600 dark:text-orange-400">
				{m.serverPage_ftpPlaintextNote()}
			</p>
		{/if}
	</div>
	<div>
		<Label for={`${formId}-host`}>{m.dedicatedServerDialog_host()}</Label><InputField
			id={`${formId}-host`}
			class="mt-1 w-full"
			bind:value={form.form.remote.host}
			placeholder="example.com"
		/>
	</div>
	<div class="grid grid-cols-2 gap-3">
		<div>
			<Label for={`${formId}-remote-port`}
				>{form.form.remote.protocol === 'sftp'
					? m.dedicatedServerDialog_sshPort()
					: m.dedicatedServerDialog_ftpPort()}</Label
			><InputField
				id={`${formId}-remote-port`}
				class="mt-1 w-full"
				bind:value={form.remotePort}
				inputmode="numeric"
			/>
		</div>
		<div>
			<Label for={`${formId}-username`}>{m.dedicatedServerDialog_username()}</Label><InputField
				id={`${formId}-username`}
				class="mt-1 w-full"
				bind:value={form.form.remote.username}
			/>
		</div>
	</div>
	{#if form.form.remote.protocol === 'sftp'}
		<div>
			<Label for={`${formId}-auth`}>{m.dedicatedServerDialog_authentication()}</Label>
			<Select
				id={`${formId}-auth`}
				type="single"
				triggerClass="mt-1 w-full"
				bind:value={form.form.remote.authentication}
				items={[
					{ value: 'password', label: m.dedicatedServerDialog_password() },
					{ value: 'privateKey', label: m.dedicatedServerDialog_privateKeyFile() },
					{ value: 'agent', label: m.dedicatedServerDialog_sshAgent() }
				]}
			/>
		</div>
	{/if}
	{#if form.form.remote.protocol === 'sftp' && form.form.remote.authentication === 'privateKey'}
		<PathField
			label={m.dedicatedServerDialog_privateKey()}
			bind:value={form.form.remote.privateKeyPath}
			onclick={() => form.choosePrivateKey()}
			icon="mdi:file-key"
		>
			{m.dedicatedServerDialog_privateKeyInfo()}
		</PathField>
	{/if}
	{#if form.form.remote.protocol !== 'sftp' || form.form.remote.authentication !== 'agent'}
		<SecretField
			id={`${formId}-remote-password`}
			label={form.form.remote.protocol !== 'sftp' || form.form.remote.authentication === 'password'
				? m.dedicatedServerDialog_password()
				: m.dedicatedServerDialog_keyPassphrase()}
			bind:value={form.remotePassword}
			saved={form.remoteCredentialSaved}
			rememberLabel={form.form.remote.authentication === 'privateKey'
				? m.serverPage_rememberPassphrase()
				: m.dedicatedServerDialog_rememberPassword()}
			bind:remember={form.rememberRemotePassword}
		/>
	{/if}
	<div>
		<Label for={`${formId}-directory`}>{m.dedicatedServerDialog_directory()}</Label><InputField
			id={`${formId}-directory`}
			class="mt-1 w-full"
			bind:value={form.form.remote.serverDirectory}
			placeholder="/"
		/>
	</div>
	<div>
		<Button
			color="primary"
			icon="mdi:lan-connect"
			loading={form.testing}
			onclick={() => form.testConnection()}>{m.dedicatedServerDialog_test()}</Button
		>
	</div>
</div>
