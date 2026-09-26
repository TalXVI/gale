<script lang="ts">
	import InputField from '$lib/components/ui/InputField.svelte';
	import Select from '$lib/components/ui/Select.svelte';
	import Checkbox from '$lib/components/ui/Checkbox.svelte';
	import Label from '$lib/components/ui/Label.svelte';
	import Button from '$lib/components/ui/Button.svelte';
	import Info from '$lib/components/ui/Info.svelte';
	import InfoBox from '$lib/components/ui/InfoBox.svelte';
	import SecretField from './SecretField.svelte';
	import { m } from '$lib/paraglide/messages';
	import type { ServerFormState } from './serverForm.svelte';

	let { form }: { form: ServerFormState } = $props();
	const formId = $props.id();

	const servicePillClass = $derived(
		form.localWorker?.service === 'running'
			? 'bg-green-100 text-green-700 dark:bg-green-900 dark:text-green-300'
			: form.localWorker?.service === 'startPending' || form.localWorker?.service === 'stopPending'
				? 'bg-yellow-100 text-yellow-700 dark:bg-yellow-900 dark:text-yellow-300'
				: 'bg-primary-200 text-primary-600 dark:bg-primary-700 dark:text-primary-300'
	);
</script>

<div class="mt-2 flex flex-col gap-3">
	<div>
		<Label for={`${formId}-sync-mode`}>{m.dedicatedServerDialog_syncMode()}</Label>
		<Select
			id={`${formId}-sync-mode`}
			type="single"
			triggerClass="mt-1 w-full"
			bind:value={form.syncChoice}
			items={[
				{ value: 'local', label: m.dedicatedServerDialog_syncModeLocal() },
				...(form.localWorker?.supported
					? [
							{
								value: 'hostedWorker',
								label: m.dedicatedServerDialog_syncModeHostedWorker()
							}
						]
					: []),
				{ value: 'worker', label: m.dedicatedServerDialog_syncModeWorker() }
			]}
		/>
		<p class="text-primary-500 mt-1 text-sm">
			{form.syncChoice === 'worker'
				? m.dedicatedServerDialog_syncModeWorkerInfo()
				: form.syncChoice === 'hostedWorker'
					? m.dedicatedServerDialog_syncModeHostedWorkerInfo()
					: m.dedicatedServerDialog_syncModeLocalInfo()}
		</p>
	</div>

	{#if form.syncChoice === 'hostedWorker'}
		{#if form.localWorker?.ownership === 'foreign'}
			<InfoBox type="warning">{m.dedicatedServerDialog_localWorkerForeign()}</InfoBox>
		{:else if form.localWorker === null || form.localWorker.service === 'notInstalled'}
			<InfoBox type="info">{m.dedicatedServerDialog_localWorkerProvisionInfo()}</InfoBox>
			<div>
				<Button
					color="primary"
					icon="mdi:server-plus"
					loading={form.provisioning}
					onclick={() => form.provisionWorker()}
					>{m.dedicatedServerDialog_localWorkerProvision()}</Button
				>
			</div>
			{#if form.provisioning}
				<p class="text-primary-500 text-sm">
					{m.dedicatedServerDialog_localWorkerProvisioning()}
				</p>
			{/if}
		{:else}
			<div class="flex flex-col gap-2">
				<span
					class={[
						'inline-flex w-fit items-center gap-1.5 rounded-full px-2.5 py-0.5 text-sm font-medium capitalize',
						servicePillClass
					]}
					title={form.localWorker.binding?.address}
				>
					{form.localWorkerStateLabel(form.localWorker.service)}
				</span>
				{#if form.localWorker.ownership === 'incomplete'}
					<InfoBox type="warning">{m.dedicatedServerDialog_localWorkerIncomplete()}</InfoBox>
					<div>
						<Button
							color="primary"
							icon="mdi:server-plus"
							loading={form.provisioning}
							onclick={() => form.provisionWorker()}
							>{m.dedicatedServerDialog_localWorkerFinishSetup()}</Button
						>
					</div>
				{/if}
				{#if form.localWorker.stoppedForShutdown}
					<InfoBox type="info">{m.dedicatedServerDialog_localWorkerShutdown()}</InfoBox>
				{:else if form.localWorker.service !== 'running' && form.localWorker.run?.phase === 'running'}
					<InfoBox type="warning">{m.dedicatedServerDialog_localWorkerCrash()}</InfoBox>
				{:else if form.localWorker.service !== 'running'}
					<InfoBox type="info">{m.dedicatedServerDialog_localWorkerOffline()}</InfoBox>
				{:else if form.localWorker.worker === null && form.localWorker.workerError}
					<InfoBox type="warning">{form.localWorker.workerError}</InfoBox>
				{/if}
				{#if form.localWorker.worker?.pendingRevision}
					<InfoBox type="info">{form.pendingLabel(form.localWorker.worker)}</InfoBox>
				{/if}
				{#if form.localWorker.updateAvailable}
					<InfoBox type="info">{m.dedicatedServerDialog_localWorkerUpdateInfo()}</InfoBox>
				{/if}
				{#each form.localWorker.warnings as warning (warning)}
					<InfoBox type="warning">{warning}</InfoBox>
				{/each}
				<div class="flex flex-wrap gap-2">
					{#if form.localWorker.service === 'stopped'}
						<Button
							color="primary"
							icon="mdi:play"
							loading={form.workerBusy}
							onclick={() => form.controlWorker('start')}
							>{m.dedicatedServerDialog_localWorkerStart()}</Button
						>
					{:else if form.localWorker.service === 'running'}
						<Button
							color="primary"
							icon="mdi:stop"
							loading={form.workerBusy}
							onclick={() => form.controlWorker('stop')}
							>{m.dedicatedServerDialog_localWorkerStop()}</Button
						>
						<Button
							color="primary"
							icon="mdi:restart"
							loading={form.workerBusy}
							onclick={() => form.controlWorker('restart')}
							>{m.dedicatedServerDialog_localWorkerRestart()}</Button
						>
					{/if}
					{#if form.localWorker.updateAvailable}
						<Button
							color="primary"
							icon="mdi:update"
							loading={form.workerBusy}
							onclick={() => form.updateWorker()}
							>{m.dedicatedServerDialog_localWorkerUpdate()}</Button
						>
					{/if}
					<Button
						class="ml-auto"
						color="red"
						icon="mdi:delete"
						loading={form.workerBusy}
						onclick={() => form.uninstallWorker()}
						>{m.dedicatedServerDialog_localWorkerUninstall()}</Button
					>
				</div>
			</div>
		{/if}
	{:else if form.syncChoice === 'worker'}
		<div>
			<Label for={`${formId}-worker-address`}>{m.dedicatedServerDialog_workerAddress()}</Label
			><InputField
				id={`${formId}-worker-address`}
				class="mt-1 w-full"
				bind:value={form.form.remote.worker.address}
				placeholder="https://worker.example.com"
			/>
		</div>
		<SecretField
			id={`${formId}-worker-token`}
			label={m.dedicatedServerDialog_workerToken()}
			bind:value={form.workerToken}
			saved={form.savedCredentials?.workerToken ?? false}
			rememberLabel={m.serverPage_rememberToken()}
			bind:remember={form.rememberWorkerToken}
		/>
		<div>
			<Button
				color="primary"
				icon="mdi:lan-connect"
				loading={form.testingWorker}
				onclick={() => form.testWorker()}>{m.dedicatedServerDialog_testWorker()}</Button
			>
		</div>
	{/if}
	{#if form.syncChoice !== 'local'}
		<div class="flex items-center">
			<Label inline for={`${formId}-auto-deploy-mods`}
				>{m.dedicatedServerDialog_workerAutoDeployMods()}</Label
			><Info>{m.dedicatedServerDialog_workerAutoDeployModsInfo()}</Info><Checkbox
				id={`${formId}-auto-deploy-mods`}
				bind:checked={form.form.remote.worker.autoDeployMods}
			/>
		</div>
		<div>
			<Label inline for={`${formId}-auto-restart`}>{m.serverPage_autoRestart()}</Label><Info
				>{m.serverPage_autoRestartInfo()}</Info
			>
			<Select
				id={`${formId}-auto-restart`}
				type="single"
				triggerClass="mt-1 w-full"
				bind:value={form.form.remote.restartPolicy}
				items={[
					{ value: 'manual', label: m.dedicatedServerDialog_restartManual() },
					{ value: 'immediate', label: m.dedicatedServerDialog_restartImmediate() },
					{ value: 'whenEmpty', label: m.dedicatedServerDialog_restartWhenEmpty() }
				]}
			/>
		</div>
	{/if}
</div>
