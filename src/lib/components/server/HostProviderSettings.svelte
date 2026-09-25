<script lang="ts">
	import InputField from '$lib/components/ui/InputField.svelte';
	import Select from '$lib/components/ui/Select.svelte';
	import Label from '$lib/components/ui/Label.svelte';
	import SecretField from './SecretField.svelte';
	import type { HostProvider } from '$lib/types';
	import { m } from '$lib/paraglide/messages';
	import type { ServerFormState } from './serverForm.svelte';

	let { form }: { form: ServerFormState } = $props();
	const formId = $props.id();
</script>

<div class="mt-2 flex flex-col gap-3">
	<div>
		<Label for={`${formId}-provider`}>{m.dedicatedServerDialog_hostProvider()}</Label>
		<Select
			id={`${formId}-provider`}
			type="single"
			triggerClass="mt-1 w-full"
			bind:value={form.form.remote.hostControl.provider}
			items={[
				{ value: 'none', label: m.dedicatedServerDialog_hostProviderNone() },
				{ value: 'datHost', label: 'DatHost' }
			]}
		/>
		<p class="text-primary-500 mt-1 text-sm">
			{m.dedicatedServerDialog_hostProviderInfo()}
		</p>
	</div>
	{#if form.form.remote.hostControl.provider === 'datHost'}
		<div>
			<Label for={`${formId}-dat-host-id`}>{m.dedicatedServerDialog_datHostServerId()}</Label
			><InputField
				id={`${formId}-dat-host-id`}
				class="mt-1 w-full"
				bind:value={form.form.remote.hostControl.datHostServerId}
			/>
		</div>
		<div>
			<Label for={`${formId}-dat-host-username`}>{m.dedicatedServerDialog_datHostUsername()}</Label
			><InputField
				id={`${formId}-dat-host-username`}
				class="mt-1 w-full"
				bind:value={form.form.remote.hostControl.datHostUsername}
			/>
		</div>
		<SecretField
			id={`${formId}-dat-host-password`}
			label={m.dedicatedServerDialog_datHostPassword()}
			bind:value={form.datHostPassword}
			saved={form.savedCredentials?.datHostPassword ?? false}
		/>
	{/if}
</div>
