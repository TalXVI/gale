<script lang="ts">
	import Dialog from '$lib/components/ui/Dialog.svelte';
	import TabsMenu from '$lib/components/ui/TabsMenu.svelte';
	import InputField from '$lib/components/ui/InputField.svelte';
	import Select from '$lib/components/ui/Select.svelte';
	import Checkbox from '$lib/components/ui/Checkbox.svelte';
	import Label from '$lib/components/ui/Label.svelte';
	import Button from '$lib/components/ui/Button.svelte';
	import Info from '$lib/components/ui/Info.svelte';
	import InfoBox from '$lib/components/ui/InfoBox.svelte';
	import PathField from '$lib/components/ui/PathField.svelte';
	import ServerSyncDialog from './ServerSyncDialog.svelte';
	import * as api from '$lib/api';
	import type {
		LocalWorkerStatus,
		PendingPublication,
		ProfileServerSettings,
		RemoteProtocol,
		RemoteServerSettings,
		RestartPolicy
	} from '$lib/types';
	import games from '$lib/state/game.svelte';
	import server from '$lib/state/server.svelte';
	import LocalServerStatus from '../misc/LocalServerStatus.svelte';
	import { Tabs } from 'bits-ui';
	import { confirm, message, open as openDialog } from '@tauri-apps/plugin-dialog';
	import { m } from '$lib/paraglide/messages';
	import { pushInfoToast } from '$lib/toast';

	const DEFAULT_SFTP_PORT = '22';
	const DEFAULT_FTP_PORT = '21';
	const FALLBACK_SERVER_PORT = 2456;
	const MAX_PORT = 65535;

	type Props = { open?: boolean };
	let { open = $bindable(false) }: Props = $props();
	const formId = $props.id();

	let gamePassword = $state('');
	let rememberGamePassword = $state(true);
	let form = $state<ProfileServerSettings>(defaultSettings());
	let port = $state('');
	let remotePort = $state(DEFAULT_SFTP_PORT);
	let remotePassword = $state('');
	/// The UI-level sync choice: 'hostedWorker' maps to syncMode 'worker'
	/// with `hosted: true`.
	let syncChoice = $state<'local' | 'hostedWorker' | 'worker'>('local');
	let localWorker = $state<LocalWorkerStatus | null>(null);
	let workerToken = $state('');
	/// The automation state the worker last confirmed (for the managed
	/// worker, what it reports live). A failed save snaps the controls
	/// back to this instead of leaving intent that never took effect.
	let savedAutomation = $state<{
		autoSync: boolean;
		autoMods: boolean;
		restartPolicy: RestartPolicy;
	}>({ autoSync: false, autoMods: false, restartPolicy: 'manual' });
	let datHostPassword = $state('');
	let rememberRemotePassword = $state(true);
	let initialized = $state(false);
	let loadingSettings = $state(false);
	let saving = $state(false);
	let launching = $state(false);
	let testing = $state(false);
	let testingWorker = $state(false);
	let provisioning = $state(false);
	let workerBusy = $state(false);
	let syncDialogOpen = $state(false);
	let syncing = $state(false);
	const busy = $derived(
		saving || launching || testing || testingWorker || provisioning || workerBusy || syncing
	);

	$effect(() => {
		if (!open) {
			initialized = false;
			gamePassword = '';
			remotePassword = '';
			workerToken = '';
			datHostPassword = '';
			return;
		}
		if (!initialized) void loadSettings();
	});

	async function refreshLocalWorker() {
		try {
			localWorker = await api.profile.server.getLocalWorkerStatus();
			// An unfinished setup would otherwise be invisible: the profile
			// still reads 'local' because linking it is the step that
			// failed. Show the hosted-worker section so Finish setup is
			// one click away.
			if (localWorker?.ownership === 'incomplete' && syncChoice === 'local') {
				syncChoice = 'hostedWorker';
			}
		} catch {
			localWorker = null;
		}
	}

	/// Initial settings built from the active game, for profiles that have
	/// never configured a dedicated server.
	function defaultSettings(): ProfileServerSettings {
		const game = games.active;
		return {
			location: 'local',
			serverName: game ? `${game.name} Server` : 'Dedicated Server',
			world: game?.slug === 'valheim' ? 'Dedicated' : '',
			port: game?.dedicatedServer?.defaultPort || FALLBACK_SERVER_PORT,
			publicServer: true,
			crossplay: false,
			extraArgs: '',
			remote: {
				protocol: 'sftp',
				host: '',
				port: Number(DEFAULT_SFTP_PORT),
				username: '',
				serverDirectory: '/',
				authentication: 'password',
				privateKeyPath: '',
				trustedHostKey: null,
				trustedCertificate: null,
				syncMode: 'local',
				worker: { address: '', hosted: false, autoSync: false, autoMods: false },
				hostControl: { provider: 'none', datHostServerId: '', datHostUsername: '' },
				restartPolicy: 'manual'
			}
		};
	}

	async function loadSettings() {
		initialized = true;
		loadingSettings = true;
		try {
			const value = (await api.profile.server.getSettings()) ?? defaultSettings();
			form = value;
			port = String(
				value.port || games.active?.dedicatedServer?.defaultPort || FALLBACK_SERVER_PORT
			);
			remotePort = String(
				value.remote.port ||
					(value.remote.protocol === 'sftp' ? DEFAULT_SFTP_PORT : DEFAULT_FTP_PORT)
			);
			syncChoice =
				value.remote.syncMode === 'worker'
					? value.remote.worker.hosted
						? 'hostedWorker'
						: 'worker'
					: 'local';
			await refreshLocalWorker();
			// The worker's journal is the authoritative automation state —
			// for the managed worker its live report wins over the stored
			// copy, which only seeds new installs.
			const liveWorker = syncChoice === 'hostedWorker' ? localWorker?.worker : null;
			form.remote.worker.autoSync = liveWorker?.autoSync ?? value.remote.worker.autoSync;
			form.remote.worker.autoMods = liveWorker?.autoMods ?? value.remote.worker.autoMods;
			form.remote.restartPolicy = liveWorker?.restartPolicy ?? value.remote.restartPolicy;
			savedAutomation = {
				autoSync: form.remote.worker.autoSync,
				autoMods: form.remote.worker.autoMods,
				restartPolicy: form.remote.restartPolicy
			};
		} finally {
			loadingSettings = false;
		}
	}

	function parsePort(value: string, label: string) {
		const parsed = Number(value);
		if (!Number.isInteger(parsed) || parsed < 1 || parsed > MAX_PORT)
			throw new Error(m.dedicatedServerDialog_portError({ label }));
		return parsed;
	}

	function remoteSettings(): RemoteServerSettings {
		return {
			...form.remote,
			host: form.remote.host.trim(),
			port: parsePort(
				remotePort,
				form.remote.protocol === 'sftp'
					? m.dedicatedServerDialog_sshPort()
					: m.dedicatedServerDialog_ftpPort()
			),
			username: form.remote.username.trim(),
			serverDirectory: form.remote.serverDirectory.trim(),
			privateKeyPath: form.remote.privateKeyPath.trim(),
			syncMode: syncChoice === 'local' ? 'local' : 'worker',
			worker: {
				...form.remote.worker,
				address: form.remote.worker.address.trim(),
				hosted: syncChoice === 'hostedWorker'
			},
			hostControl: {
				...form.remote.hostControl,
				datHostServerId: form.remote.hostControl.datHostServerId.trim(),
				datHostUsername: form.remote.hostControl.datHostUsername.trim()
			}
		};
	}

	function changeRemoteProtocol(value: RemoteProtocol) {
		if (
			(form.remote.protocol === 'sftp' && remotePort === DEFAULT_SFTP_PORT) ||
			(form.remote.protocol !== 'sftp' && remotePort === DEFAULT_FTP_PORT)
		) {
			remotePort = value === 'sftp' ? DEFAULT_SFTP_PORT : DEFAULT_FTP_PORT;
		}
		form.remote.protocol = value;
		form.remote.trustedHostKey = null;
		form.remote.trustedCertificate = null;
	}

	async function choosePrivateKey() {
		const selected = await openDialog({
			title: m.dedicatedServerDialog_privateKeyTitle(),
			directory: false,
			multiple: false
		});
		if (typeof selected === 'string') form.remote.privateKeyPath = selected;
	}

	function settings(): ProfileServerSettings {
		return {
			...form,
			serverName: form.serverName.trim(),
			world: form.world.trim(),
			port: parsePort(port, m.dedicatedServerDialog_serverPort()),
			extraArgs: form.extraArgs.trim(),
			remote: remoteSettings()
		};
	}

	async function checkedSettings() {
		try {
			return settings();
		} catch (error) {
			await message(error instanceof Error ? error.message : String(error));
			return null;
		}
	}

	async function trustHost(fingerprint: string) {
		const accepted = await confirm(m.dedicatedServerDialog_trustMessage({ fingerprint }), {
			title: m.dedicatedServerDialog_trustTitle(),
			kind: 'warning'
		});
		if (accepted) form.remote.trustedHostKey = fingerprint;
		return accepted;
	}

	async function trustInvalidCertificate(fingerprint: string) {
		const accepted = await confirm(
			m.dedicatedServerDialog_certificateMessage({ host: form.remote.host.trim() }),
			{
				title: m.dedicatedServerDialog_certificateTitle(),
				kind: 'warning'
			}
		);
		if (accepted) form.remote.trustedCertificate = fingerprint;
		return accepted;
	}

	async function testConnection() {
		const current = await checkedSettings();
		if (!current) return;
		testing = true;
		try {
			let result = await api.profile.server.testRemoteConnection(current.remote, remotePassword);
			if (result.status === 'hostKeyUntrusted') {
				if (!(await trustHost(result.fingerprint))) return;
				current.remote.trustedHostKey = form.remote.trustedHostKey;
				result = await api.profile.server.testRemoteConnection(current.remote, remotePassword);
			}
			if (result.status === 'certificateUntrusted') {
				if (!(await trustInvalidCertificate(result.fingerprint))) return;
				current.remote.trustedCertificate = form.remote.trustedCertificate;
				result = await api.profile.server.testRemoteConnection(current.remote, remotePassword);
			}
			if (result.status !== 'connected') return;
			await message(
				!result.encrypted
					? m.dedicatedServerDialog_connectionPlain({ host: form.remote.host })
					: form.remote.protocol !== 'sftp' && form.remote.trustedCertificate
						? m.dedicatedServerDialog_connectionEncrypted({ host: form.remote.host })
						: m.dedicatedServerDialog_connectionSecure({ host: form.remote.host }),
				{
					title: m.dedicatedServerDialog_connectionTitle(),
					kind: 'info'
				}
			);
		} finally {
			testing = false;
		}
	}

	async function testWorker() {
		const current = await checkedSettings();
		if (!current) return;
		testingWorker = true;
		try {
			const status = await api.profile.server.testWorkerConnection(current.remote, workerToken);
			await message(
				m.dedicatedServerDialog_workerConnected({
					workerId: status.workerId,
					autoSync: status.autoSync
						? m.dedicatedServerDialog_workerAutoOn()
						: m.dedicatedServerDialog_workerAutoOff()
				}),
				{ title: m.dedicatedServerDialog_connectionTitle(), kind: 'info' }
			);
		} finally {
			testingWorker = false;
		}
	}

	async function save() {
		const current = await checkedSettings();
		if (!current) return;
		saving = true;
		try {
			await api.profile.server.setSettings(
				current,
				remotePassword,
				workerToken,
				datHostPassword,
				rememberRemotePassword,
				gamePassword,
				rememberGamePassword
			);
			// Automation toggles may have changed — re-read the worker's own
			// state so the pending banner reflects what it will actually do.
			await refreshLocalWorker();
			savedAutomation = {
				autoSync: form.remote.worker.autoSync,
				autoMods: form.remote.worker.autoMods,
				restartPolicy: form.remote.restartPolicy
			};
			pushInfoToast({ message: m.dedicatedServerDialog_saved() });
		} catch {
			// The save failed — the backend leaves stored settings
			// untouched when the worker did not accept the change. Snap
			// the automation controls back to the worker's actual state.
			await refreshLocalWorker();
			reconcileAutomation();
		} finally {
			saving = false;
		}
	}

	/// Reverts the automation controls to the last state the worker
	/// confirmed — its live report for the managed worker, else the last
	/// values it acknowledged.
	function reconcileAutomation() {
		const liveWorker = syncChoice === 'hostedWorker' ? localWorker?.worker : null;
		form.remote.worker.autoSync = liveWorker?.autoSync ?? savedAutomation.autoSync;
		form.remote.worker.autoMods = liveWorker?.autoMods ?? savedAutomation.autoMods;
		form.remote.restartPolicy = liveWorker?.restartPolicy ?? savedAutomation.restartPolicy;
	}

	/// Saves the current transport settings first — provisioning derives
	/// the worker's config from the *saved* settings, so unsaved edits
	/// would otherwise leave worker and desktop pointing at different
	/// remotes.
	///
	/// The save uses `local` sync mode on purpose: a fresh profile has no
	/// worker address yet, so `worker` mode would fail validation before
	/// provisioning could start. Once the service is installed, the
	/// backend itself persists hosted-worker mode with the loopback
	/// address; this dialog then mirrors that state.
	async function provisionWorker() {
		const current = await checkedSettings();
		if (!current) return;
		current.remote.syncMode = 'local';
		current.remote.worker.hosted = false;
		provisioning = true;
		try {
			await api.profile.server.setSettings(
				current,
				remotePassword,
				workerToken,
				datHostPassword,
				rememberRemotePassword
			);
			localWorker = await api.profile.server.provisionLocalWorker(remotePassword, datHostPassword);
			if (localWorker.binding) form.remote.worker.address = localWorker.binding.address;
			// A reprovisioned worker keeps its journal — adopt whatever
			// automation flags it actually runs.
			reconcileAutomation();
			savedAutomation = {
				autoSync: form.remote.worker.autoSync,
				autoMods: form.remote.worker.autoMods,
				restartPolicy: form.remote.restartPolicy
			};
		} finally {
			provisioning = false;
		}
	}

	async function controlWorker(action: 'start' | 'stop' | 'restart') {
		workerBusy = true;
		try {
			localWorker = await api.profile.server.controlLocalWorker(action);
		} finally {
			workerBusy = false;
		}
	}

	async function updateWorker() {
		workerBusy = true;
		try {
			localWorker = await api.profile.server.updateLocalWorker();
		} finally {
			workerBusy = false;
		}
	}

	async function uninstallWorker() {
		const accepted = await confirm(m.dedicatedServerDialog_localWorkerUninstallConfirm(), {
			title: m.dedicatedServerDialog_localWorkerUninstall(),
			kind: 'warning'
		});
		if (!accepted) return;
		workerBusy = true;
		try {
			localWorker = await api.profile.server.uninstallLocalWorker();
			syncChoice = 'local';
			form.remote.worker.address = '';
		} finally {
			workerBusy = false;
		}
	}

	/// The pending banner mirrors the worker's own automation state —
	/// never the unsaved checkboxes — so it only promises an automatic
	/// deployment the worker can actually perform.
	function pendingLabel(pending: PendingPublication): string {
		switch (pending.mode) {
			case 'automatic':
				return pending.retrying
					? m.dedicatedServerDialog_localWorkerPendingRetry()
					: m.dedicatedServerDialog_localWorkerPending();
			case 'configOnly':
				if (!pending.modsOutstanding) {
					return pending.retrying
						? m.dedicatedServerDialog_localWorkerPendingConfigsRetry()
						: m.dedicatedServerDialog_localWorkerPendingConfigs();
				}
				return pending.retrying
					? m.dedicatedServerDialog_localWorkerPendingConfigOnlyRetry()
					: m.dedicatedServerDialog_localWorkerPendingConfigOnly();
			case 'modsManual':
				return m.dedicatedServerDialog_localWorkerPendingModsManual();
			case 'manual':
				return m.dedicatedServerDialog_localWorkerPendingManual();
		}
	}

	function localWorkerStateLabel(state: string | undefined): string {
		switch (state) {
			case 'running':
				return m.dedicatedServerDialog_localWorkerStateRunning();
			case 'stopped':
				return m.dedicatedServerDialog_localWorkerStateStopped();
			case 'startPending':
				return m.dedicatedServerDialog_localWorkerStateStartPending();
			case 'stopPending':
				return m.dedicatedServerDialog_localWorkerStateStopPending();
			case 'notInstalled':
				return m.dedicatedServerDialog_localWorkerStateNotInstalled();
			default:
				return m.dedicatedServerDialog_localWorkerStateOther();
		}
	}

	async function launch() {
		const current = await checkedSettings();
		if (!current) return;
		launching = true;
		try {
			await api.profile.server.launch(current, gamePassword, rememberGamePassword);
			open = false;
		} finally {
			launching = false;
		}
	}

	async function syncServer() {
		const current = await checkedSettings();
		if (!current) return;
		syncing = true;
		try {
			await api.profile.server.setSettings(
				current,
				remotePassword,
				workerToken,
				datHostPassword,
				rememberRemotePassword
			);
			syncDialogOpen = true;
		} finally {
			syncing = false;
		}
	}
</script>

<Dialog title={m.dedicatedServerDialog_title()} bind:open canClose={!busy} large>
	<p class="text-primary-600 dark:text-primary-300 mt-1">
		{m.dedicatedServerDialog_content()}
	</p>

	<div class="mt-3"><LocalServerStatus /></div>
	{#if loadingSettings}
		<div class="text-primary-500 mt-5">{m.dedicatedServerDialog_loading()}</div>
	{:else}
		<fieldset disabled={busy}>
			<TabsMenu
				bind:value={form.location}
				options={[
					{ value: 'local', label: m.dedicatedServerDialog_locationLocal() },
					{ value: 'remote', label: m.dedicatedServerDialog_locationRemote() }
				]}
			>
				<Tabs.Content value="local">
					<div class="mt-4 flex flex-col gap-3">
						<div>
							<Label for={`${formId}-field-1`}>{m.dedicatedServerDialog_serverName()}</Label
							><InputField
								id={`${formId}-field-1`}
								class="mt-1 w-full"
								bind:value={form.serverName}
								placeholder={m.dedicatedServerDialog_serverNamePlaceholder()}
							/>
						</div>
						<div>
							<Label for={`${formId}-field-2`}>{m.dedicatedServerDialog_world()}</Label><InputField
								id={`${formId}-field-2`}
								class="mt-1 w-full"
								bind:value={form.world}
								placeholder={m.dedicatedServerDialog_worldPlaceholder()}
							/>
						</div>
						<div>
							<Label for={`${formId}-field-3`}>{m.dedicatedServerDialog_password()}</Label
							><InputField
								id={`${formId}-field-3`}
								class="mt-1 w-full"
								aria-describedby={`${formId}-password-help`}
								bind:value={gamePassword}
								type="password"
							/>
							<p id={`${formId}-password-help`} class="text-primary-500 mt-1 text-sm">
								{rememberGamePassword
									? m.dedicatedServerDialog_savedPassword()
									: m.dedicatedServerDialog_sessionPassword()}
							</p>
						</div>
						<div class="flex items-center">
							<Label for={`${formId}-field-4`}>{m.dedicatedServerDialog_rememberPassword()}</Label
							><Info>{m.dedicatedServerDialog_credentialInfo()}</Info><Checkbox
								id={`${formId}-field-4`}
								bind:checked={rememberGamePassword}
							/>
						</div>
						<div>
							<Label for={`${formId}-field-5`}>{m.dedicatedServerDialog_serverPort()}</Label
							><InputField
								id={`${formId}-field-5`}
								class="mt-1 w-full"
								bind:value={port}
								inputmode="numeric"
							/>
						</div>
						<div class="flex items-center">
							<Label for={`${formId}-field-6`}>{m.dedicatedServerDialog_public()}</Label><Info
								>{m.dedicatedServerDialog_publicInfo()}</Info
							><Checkbox id={`${formId}-field-6`} bind:checked={form.publicServer} />
						</div>
						<div class="flex items-center">
							<Label for={`${formId}-field-7`}>{m.dedicatedServerDialog_crossplay()}</Label><Info
								>{m.dedicatedServerDialog_crossplayInfo()}</Info
							><Checkbox id={`${formId}-field-7`} bind:checked={form.crossplay} />
						</div>
					</div>
				</Tabs.Content>

				<Tabs.Content value="remote">
					<div class="mt-4 flex flex-col gap-3">
						<InfoBox type={form.remote.protocol === 'ftp' ? 'warning' : 'info'}
							>{form.remote.protocol === 'sftp'
								? m.dedicatedServerDialog_sftpInfo()
								: form.remote.protocol === 'ftps'
									? m.dedicatedServerDialog_ftpsInfo()
									: m.dedicatedServerDialog_ftpInfo()}</InfoBox
						>
						<div>
							<Label for={`${formId}-field-8`}>{m.dedicatedServerDialog_protocol()}</Label>
							<Select
								id={`${formId}-field-8`}
								type="single"
								triggerClass="mt-1 w-full"
								value={form.remote.protocol}
								onValueChange={(value) => changeRemoteProtocol(value as RemoteProtocol)}
								items={[
									{ value: 'sftp', label: m.dedicatedServerDialog_protocolSftp() },
									{ value: 'ftps', label: m.dedicatedServerDialog_protocolFtps() },
									{ value: 'ftp', label: m.dedicatedServerDialog_protocolFtp() }
								]}
							/>
						</div>
						<div>
							<Label for={`${formId}-field-9`}>{m.dedicatedServerDialog_host()}</Label><InputField
								id={`${formId}-field-9`}
								class="mt-1 w-full"
								bind:value={form.remote.host}
								placeholder="example.com"
							/>
						</div>
						<div class="grid grid-cols-2 gap-3">
							<div>
								<Label for={`${formId}-remote-port`}
									>{form.remote.protocol === 'sftp'
										? m.dedicatedServerDialog_sshPort()
										: m.dedicatedServerDialog_ftpPort()}</Label
								><InputField
									id={`${formId}-remote-port`}
									class="mt-1 w-full"
									bind:value={remotePort}
									inputmode="numeric"
								/>
							</div>
							<div>
								<Label for={`${formId}-field-10`}>{m.dedicatedServerDialog_username()}</Label
								><InputField
									id={`${formId}-field-10`}
									class="mt-1 w-full"
									bind:value={form.remote.username}
								/>
							</div>
						</div>
						{#if form.remote.protocol === 'sftp'}
							<div>
								<Label for={`${formId}-field-11`}>{m.dedicatedServerDialog_authentication()}</Label>
								<Select
									id={`${formId}-field-11`}
									type="single"
									triggerClass="mt-1 w-full"
									bind:value={form.remote.authentication}
									items={[
										{ value: 'password', label: m.dedicatedServerDialog_password() },
										{ value: 'privateKey', label: m.dedicatedServerDialog_privateKeyFile() },
										{ value: 'agent', label: m.dedicatedServerDialog_sshAgent() }
									]}
								/>
							</div>
						{/if}
						{#if form.remote.protocol === 'sftp' && form.remote.authentication === 'privateKey'}
							<PathField
								label={m.dedicatedServerDialog_privateKey()}
								bind:value={form.remote.privateKeyPath}
								onclick={choosePrivateKey}
								icon="mdi:file-key"
							>
								{m.dedicatedServerDialog_privateKeyInfo()}
							</PathField>
						{/if}
						{#if form.remote.protocol !== 'sftp' || form.remote.authentication !== 'agent'}
							<div>
								<Label for={`${formId}-remote-password`}
									>{form.remote.protocol !== 'sftp' || form.remote.authentication === 'password'
										? m.dedicatedServerDialog_password()
										: m.dedicatedServerDialog_keyPassphrase()}</Label
								>
								<InputField
									id={`${formId}-remote-password`}
									class="mt-1 w-full"
									bind:value={remotePassword}
									type="password"
								/>
								<p class="text-primary-500 mt-1 text-sm">
									{form.remote.protocol !== 'sftp' || form.remote.authentication === 'password'
										? m.dedicatedServerDialog_savedPassword()
										: m.dedicatedServerDialog_remoteSavedPassphrase()}
								</p>
							</div>
						{/if}
						<div>
							<Label for={`${formId}-field-12`}>{m.dedicatedServerDialog_directory()}</Label
							><InputField
								id={`${formId}-field-12`}
								class="mt-1 w-full"
								bind:value={form.remote.serverDirectory}
								placeholder="/home/valheim/server"
							/>
						</div>
						<div>
							<Button
								color="primary"
								icon="mdi:lan-connect"
								loading={testing}
								onclick={testConnection}>{m.dedicatedServerDialog_test()}</Button
							>
						</div>

						<p class="text-primary-600 dark:text-primary-300 text-sm">
							{m.dedicatedServerDialog_testUnsaved()}
						</p>
						<div class="border-primary-300 dark:border-primary-600 mt-2 border-t pt-3">
							<Label for={`${formId}-field-13`}>{m.dedicatedServerDialog_syncMode()}</Label>
							<Select
								id={`${formId}-field-13`}
								type="single"
								triggerClass="mt-1 w-full"
								bind:value={syncChoice}
								items={[
									{ value: 'local', label: m.dedicatedServerDialog_syncModeLocal() },
									...(localWorker?.supported
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
								{syncChoice === 'worker'
									? m.dedicatedServerDialog_syncModeWorkerInfo()
									: syncChoice === 'hostedWorker'
										? m.dedicatedServerDialog_syncModeHostedWorkerInfo()
										: m.dedicatedServerDialog_syncModeLocalInfo()}
							</p>
						</div>

						{#if syncChoice === 'hostedWorker'}
							{#if localWorker?.ownership === 'foreign'}
								<InfoBox type="warning">{m.dedicatedServerDialog_localWorkerForeign()}</InfoBox>
							{:else if localWorker === null || localWorker.service === 'notInstalled'}
								<InfoBox type="info">{m.dedicatedServerDialog_localWorkerProvisionInfo()}</InfoBox>
								<div>
									<Button
										color="primary"
										icon="mdi:server-plus"
										loading={provisioning}
										onclick={provisionWorker}
										>{m.dedicatedServerDialog_localWorkerProvision()}</Button
									>
								</div>
								{#if provisioning}
									<p class="text-primary-500 text-sm">
										{m.dedicatedServerDialog_localWorkerProvisioning()}
									</p>
								{/if}
							{:else}
								<div class="flex flex-col gap-2">
									<p class="text-primary-600 dark:text-primary-300 text-sm">
										{m.dedicatedServerDialog_localWorkerStatus({
											state: localWorkerStateLabel(localWorker.service)
										})}
										{#if localWorker.binding}
											— {localWorker.binding.address}
										{/if}
									</p>
									{#if localWorker.ownership === 'incomplete'}
										<InfoBox type="warning"
											>{m.dedicatedServerDialog_localWorkerIncomplete()}</InfoBox
										>
										<div>
											<Button
												color="primary"
												icon="mdi:server-plus"
												loading={provisioning}
												onclick={provisionWorker}
												>{m.dedicatedServerDialog_localWorkerFinishSetup()}</Button
											>
										</div>
									{/if}
									{#if localWorker.stoppedForShutdown}
										<InfoBox type="info">{m.dedicatedServerDialog_localWorkerShutdown()}</InfoBox>
									{:else if localWorker.service !== 'running' && localWorker.run?.phase === 'running'}
										<InfoBox type="warning">{m.dedicatedServerDialog_localWorkerCrash()}</InfoBox>
									{:else if localWorker.service !== 'running'}
										<InfoBox type="info">{m.dedicatedServerDialog_localWorkerOffline()}</InfoBox>
									{:else if localWorker.worker === null && localWorker.workerError}
										<InfoBox type="warning">{localWorker.workerError}</InfoBox>
									{/if}
									{#if localWorker.pendingPublication}
										<InfoBox type="info">{pendingLabel(localWorker.pendingPublication)}</InfoBox>
									{/if}
									{#if localWorker.updateAvailable}
										<InfoBox type="info">{m.dedicatedServerDialog_localWorkerUpdateInfo()}</InfoBox>
									{/if}
									{#each localWorker.warnings as warning (warning)}
										<InfoBox type="warning">{warning}</InfoBox>
									{/each}
									<div class="flex flex-wrap gap-2">
										{#if localWorker.service === 'stopped'}
											<Button
												color="primary"
												icon="mdi:play"
												loading={workerBusy}
												onclick={() => controlWorker('start')}
												>{m.dedicatedServerDialog_localWorkerStart()}</Button
											>
										{:else if localWorker.service === 'running'}
											<Button
												color="primary"
												icon="mdi:stop"
												loading={workerBusy}
												onclick={() => controlWorker('stop')}
												>{m.dedicatedServerDialog_localWorkerStop()}</Button
											>
											<Button
												color="primary"
												icon="mdi:restart"
												loading={workerBusy}
												onclick={() => controlWorker('restart')}
												>{m.dedicatedServerDialog_localWorkerRestart()}</Button
											>
										{/if}
										{#if localWorker.updateAvailable}
											<Button
												color="primary"
												icon="mdi:update"
												loading={workerBusy}
												onclick={updateWorker}>{m.dedicatedServerDialog_localWorkerUpdate()}</Button
											>
										{/if}
										<Button
											color="primary"
											icon="mdi:delete"
											loading={workerBusy}
											onclick={uninstallWorker}
											>{m.dedicatedServerDialog_localWorkerUninstall()}</Button
										>
									</div>
								</div>
							{/if}
						{:else if syncChoice === 'worker'}
							<div>
								<Label for={`${formId}-field-14`}>{m.dedicatedServerDialog_workerAddress()}</Label
								><InputField
									id={`${formId}-field-14`}
									class="mt-1 w-full"
									bind:value={form.remote.worker.address}
									placeholder="https://worker.example.com"
								/>
							</div>
							<div>
								<Label for={`${formId}-field-15`}>{m.dedicatedServerDialog_workerToken()}</Label
								><InputField
									id={`${formId}-field-15`}
									class="mt-1 w-full"
									bind:value={workerToken}
									type="password"
								/>
								<p class="text-primary-500 mt-1 text-sm">
									{m.dedicatedServerDialog_savedPassword()}
								</p>
							</div>
							<div>
								<Button
									color="primary"
									icon="mdi:lan-connect"
									loading={testingWorker}
									onclick={testWorker}>{m.dedicatedServerDialog_testWorker()}</Button
								>
							</div>
						{/if}
						{#if syncChoice !== 'local'}
							<div class="flex items-center">
								<Label for={`${formId}-field-16`}>{m.dedicatedServerDialog_workerAutoSync()}</Label
								><Info>{m.dedicatedServerDialog_workerAutoSyncInfo()}</Info><Checkbox
									id={`${formId}-field-16`}
									bind:checked={form.remote.worker.autoSync}
								/>
							</div>
							<div class="flex items-center">
								<Label for={`${formId}-field-17`}>{m.dedicatedServerDialog_workerAutoMods()}</Label
								><Info>{m.dedicatedServerDialog_workerAutoModsInfo()}</Info><Checkbox
									id={`${formId}-field-17`}
									bind:checked={form.remote.worker.autoMods}
								/>
							</div>
						{/if}

						<div>
							<Label for={`${formId}-field-18`}>{m.dedicatedServerDialog_hostProvider()}</Label>
							<Select
								id={`${formId}-field-18`}
								type="single"
								triggerClass="mt-1 w-full"
								bind:value={form.remote.hostControl.provider}
								items={[
									{ value: 'none', label: m.dedicatedServerDialog_hostProviderNone() },
									{ value: 'datHost', label: 'DatHost' }
								]}
							/>
							<p class="text-primary-500 mt-1 text-sm">
								{m.dedicatedServerDialog_hostProviderInfo()}
							</p>
						</div>
						{#if form.remote.hostControl.provider === 'datHost'}
							<div>
								<Label for={`${formId}-field-19`}>{m.dedicatedServerDialog_datHostServerId()}</Label
								><InputField
									id={`${formId}-field-19`}
									class="mt-1 w-full"
									bind:value={form.remote.hostControl.datHostServerId}
								/>
							</div>
							<div>
								<Label for={`${formId}-field-20`}>{m.dedicatedServerDialog_datHostUsername()}</Label
								><InputField
									id={`${formId}-field-20`}
									class="mt-1 w-full"
									bind:value={form.remote.hostControl.datHostUsername}
									placeholder="you@example.com"
								/>
							</div>
							<div>
								<Label for={`${formId}-field-21`}>{m.dedicatedServerDialog_datHostPassword()}</Label
								><InputField
									id={`${formId}-field-21`}
									class="mt-1 w-full"
									bind:value={datHostPassword}
									type="password"
								/>
								<p class="text-primary-500 mt-1 text-sm">
									{m.dedicatedServerDialog_savedPassword()}
								</p>
							</div>
						{/if}
						<div>
							<Label for={`${formId}-field-22`}>{m.dedicatedServerDialog_restartPolicy()}</Label>
							<Select
								id={`${formId}-field-22`}
								type="single"
								triggerClass="mt-1 w-full"
								bind:value={form.remote.restartPolicy}
								items={[
									{ value: 'manual', label: m.dedicatedServerDialog_restartManual() },
									{ value: 'immediate', label: m.dedicatedServerDialog_restartImmediate() },
									{ value: 'whenEmpty', label: m.dedicatedServerDialog_restartWhenEmpty() }
								]}
							/>
							<p class="text-primary-500 mt-1 text-sm">
								{m.dedicatedServerDialog_restartPolicyInfo()}
							</p>
						</div>
						<div class="flex items-center">
							<Label for={`${formId}-field-23`}>{m.dedicatedServerDialog_rememberPassword()}</Label>
							<Info>{m.dedicatedServerDialog_credentialInfo()}</Info>
							<Checkbox id={`${formId}-field-23`} bind:checked={rememberRemotePassword} />
						</div>
					</div>
				</Tabs.Content>
			</TabsMenu>

			<details class="mt-4">
				<summary class="text-primary-600 dark:text-primary-300 cursor-pointer"
					>{m.dedicatedServerDialog_advancedOptions()}</summary
				>
				<div class="mt-2">
					<Label for={`${formId}-field-24`}>{m.dedicatedServerDialog_additionalArgs()}</Label
					><InputField
						id={`${formId}-field-24`}
						class="mt-1 w-full"
						bind:value={form.extraArgs}
						placeholder="-savedir ..."
					/>
				</div>
			</details>
		</fieldset>
	{/if}

	<div class="mt-5 flex w-full flex-wrap items-center justify-end gap-2">
		<Button color="primary" disabled={busy} onclick={() => (open = false)}
			>{m.dedicatedServerDialog_cancel()}</Button
		>
		<Button
			color="primary"
			icon="mdi:content-save"
			disabled={busy || loadingSettings}
			loading={saving}
			onclick={save}>{m.dedicatedServerDialog_save()}</Button
		>
		{#if form.location === 'local'}
			<Button
				icon="mdi:server"
				disabled={busy || loadingSettings || server.status.state === 'running'}
				loading={launching}
				onclick={launch}>{m.dedicatedServerDialog_launch()}</Button
			>
		{:else}
			<Button
				icon="mdi:sync"
				disabled={busy || loadingSettings}
				loading={syncing}
				onclick={syncServer}>{m.dedicatedServerDialog_sync()}</Button
			>
		{/if}
	</div>
</Dialog>

<ServerSyncDialog bind:open={syncDialogOpen} />
