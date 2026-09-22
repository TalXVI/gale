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
		HostProvider,
		LocalWorkerStatus,
		PendingPublication,
		ProfileServerSettings,
		RemoteAuthentication,
		RemoteProtocol,
		RemoteServerSettings,
		RestartPolicy,
		ServerLocation
	} from '$lib/types';
	import games from '$lib/state/game.svelte';
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

	let location = $state<ServerLocation>('local');
	let serverName = $state('');
	let worldName = $state('');
	let gamePassword = $state('');
	let rememberGamePassword = $state(true);
	let port = $state('');
	let publicServer = $state(true);
	let crossplay = $state(false);
	let extraArgs = $state('');
	let remoteHost = $state('');
	let remoteProtocol = $state<RemoteProtocol>('sftp');
	let remotePort = $state(DEFAULT_SFTP_PORT);
	let remoteUser = $state('');
	let remotePath = $state('');
	let remoteAuthentication = $state<RemoteAuthentication>('password');
	let privateKeyPath = $state('');
	let trustedHostKey = $state<string | null>(null);
	let trustedCertificate = $state<string | null>(null);
	let remotePassword = $state('');
	/// The UI-level sync choice: 'hostedWorker' maps to syncMode 'worker'
	/// with `hosted: true`.
	let syncChoice = $state<'local' | 'hostedWorker' | 'worker'>('local');
	let localWorker = $state<LocalWorkerStatus | null>(null);
	let workerAddress = $state('');
	let workerToken = $state('');
	let workerAutoSync = $state(false);
	let workerAutoMods = $state(false);
	let hostProvider = $state<HostProvider>('none');
	let datHostServerId = $state('');
	let datHostUsername = $state('');
	let datHostPassword = $state('');
	let restartPolicy = $state<RestartPolicy>('manual');
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
			location = value.location;
			serverName = value.serverName;
			worldName = value.world;
			port = String(
				value.port || games.active?.dedicatedServer?.defaultPort || FALLBACK_SERVER_PORT
			);
			publicServer = value.publicServer;
			crossplay = value.crossplay;
			extraArgs = value.extraArgs;
			remoteHost = value.remote.host;
			remoteProtocol = value.remote.protocol;
			remotePort = String(
				value.remote.port || (remoteProtocol === 'sftp' ? DEFAULT_SFTP_PORT : DEFAULT_FTP_PORT)
			);
			remoteUser = value.remote.username;
			remotePath = value.remote.serverDirectory;
			remoteAuthentication = value.remote.authentication;
			privateKeyPath = value.remote.privateKeyPath;
			trustedHostKey = value.remote.trustedHostKey;
			trustedCertificate = value.remote.trustedCertificate;
			syncChoice =
				value.remote.syncMode === 'worker'
					? value.remote.worker.hosted
						? 'hostedWorker'
						: 'worker'
					: 'local';
			workerAddress = value.remote.worker.address;
			workerAutoSync = value.remote.worker.autoSync;
			workerAutoMods = value.remote.worker.autoMods;
			void refreshLocalWorker();
			hostProvider = value.remote.hostControl.provider;
			datHostServerId = value.remote.hostControl.datHostServerId;
			datHostUsername = value.remote.hostControl.datHostUsername;
			restartPolicy = value.remote.restartPolicy;
		} finally {
			loadingSettings = false;
		}
	}

	function parsePort(value: string, label: string) {
		const parsed = Number.parseInt(value, 10);
		if (!Number.isInteger(parsed) || parsed < 1 || parsed > MAX_PORT)
			throw new Error(m.dedicatedServerDialog_portError({ label }));
		return parsed;
	}

	function remoteSettings(): RemoteServerSettings {
		return {
			protocol: remoteProtocol,
			host: remoteHost.trim(),
			port: parsePort(
				remotePort,
				remoteProtocol === 'sftp'
					? m.dedicatedServerDialog_sshPort()
					: m.dedicatedServerDialog_ftpPort()
			),
			username: remoteUser.trim(),
			serverDirectory: remotePath.trim(),
			authentication: remoteAuthentication,
			privateKeyPath: privateKeyPath.trim(),
			trustedHostKey,
			trustedCertificate,
			syncMode: syncChoice === 'local' ? 'local' : 'worker',
			worker: {
				address: workerAddress.trim(),
				hosted: syncChoice === 'hostedWorker',
				autoSync: workerAutoSync,
				autoMods: workerAutoMods
			},
			hostControl: {
				provider: hostProvider,
				datHostServerId: datHostServerId.trim(),
				datHostUsername: datHostUsername.trim()
			},
			restartPolicy
		};
	}

	function changeRemoteProtocol(value: RemoteProtocol) {
		if (
			(remoteProtocol === 'sftp' && remotePort === DEFAULT_SFTP_PORT) ||
			(remoteProtocol !== 'sftp' && remotePort === DEFAULT_FTP_PORT)
		) {
			remotePort = value === 'sftp' ? DEFAULT_SFTP_PORT : DEFAULT_FTP_PORT;
		}
		remoteProtocol = value;
		trustedHostKey = null;
		trustedCertificate = null;
	}

	async function choosePrivateKey() {
		const selected = await openDialog({
			title: m.dedicatedServerDialog_privateKeyTitle(),
			directory: false,
			multiple: false
		});
		if (typeof selected === 'string') privateKeyPath = selected;
	}

	function settings(): ProfileServerSettings {
		return {
			location,
			serverName: serverName.trim(),
			world: worldName.trim(),
			port: parsePort(port, m.dedicatedServerDialog_serverPort()),
			publicServer,
			crossplay,
			extraArgs: extraArgs.trim(),
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
		if (accepted) trustedHostKey = fingerprint;
		return accepted;
	}

	async function trustInvalidCertificate(fingerprint: string) {
		const accepted = await confirm(
			m.dedicatedServerDialog_certificateMessage({ host: remoteHost.trim() }),
			{
				title: m.dedicatedServerDialog_certificateTitle(),
				kind: 'warning'
			}
		);
		if (accepted) trustedCertificate = fingerprint;
		return accepted;
	}

	async function testConnection() {
		const current = await checkedSettings();
		if (!current) return;
		testing = true;
		try {
			let result = await api.profile.server.testRemoteConnection(
				current.remote,
				remotePassword,
				datHostPassword,
				rememberRemotePassword
			);
			if (result.status === 'hostKeyUntrusted') {
				if (!(await trustHost(result.fingerprint))) return;
				current.remote.trustedHostKey = trustedHostKey;
				result = await api.profile.server.testRemoteConnection(
					current.remote,
					remotePassword,
					datHostPassword,
					rememberRemotePassword
				);
			}
			if (result.status === 'certificateUntrusted') {
				if (!(await trustInvalidCertificate(result.fingerprint))) return;
				current.remote.trustedCertificate = trustedCertificate;
				result = await api.profile.server.testRemoteConnection(
					current.remote,
					remotePassword,
					datHostPassword,
					rememberRemotePassword
				);
			}
			if (result.status !== 'connected') return;
			await message(
				!result.encrypted
					? m.dedicatedServerDialog_connectionPlain({ host: remoteHost })
					: remoteProtocol !== 'sftp' && trustedCertificate
						? m.dedicatedServerDialog_connectionEncrypted({ host: remoteHost })
						: m.dedicatedServerDialog_connectionSecure({ host: remoteHost }),
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
			const status = await api.profile.server.testWorkerConnection(
				current.remote,
				workerToken,
				rememberRemotePassword
			);
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
				rememberRemotePassword
			);
			// Automation toggles may have changed — re-read the worker's own
			// state so the pending banner reflects what it will actually do.
			await refreshLocalWorker();
			pushInfoToast({ message: m.dedicatedServerDialog_saved() });
		} finally {
			saving = false;
		}
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
			if (localWorker.binding) workerAddress = localWorker.binding.address;
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
			workerAddress = '';
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
				return pending.retrying
					? m.dedicatedServerDialog_localWorkerPendingConfigOnlyRetry()
					: m.dedicatedServerDialog_localWorkerPendingConfigOnly();
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
		await api.profile.server.setSettings(
			current,
			remotePassword,
			workerToken,
			datHostPassword,
			rememberRemotePassword
		);
		syncDialogOpen = true;
	}
</script>

<Dialog title={m.dedicatedServerDialog_title()} bind:open large>
	<p class="text-primary-600 dark:text-primary-300 mt-1">
		{m.dedicatedServerDialog_content()}
	</p>

	{#if loadingSettings}
		<div class="text-primary-500 mt-5">{m.dedicatedServerDialog_loading()}</div>
	{:else}
		<TabsMenu
			bind:value={location}
			options={[
				{ value: 'local', label: m.dedicatedServerDialog_locationLocal() },
				{ value: 'remote', label: m.dedicatedServerDialog_locationRemote() }
			]}
		>
			<Tabs.Content value="local">
				<div class="mt-4 flex flex-col gap-3">
					<div>
						<Label>{m.dedicatedServerDialog_serverName()}</Label><InputField
							class="mt-1 w-full"
							bind:value={serverName}
							placeholder={m.dedicatedServerDialog_serverNamePlaceholder()}
						/>
					</div>
					<div>
						<Label>{m.dedicatedServerDialog_world()}</Label><InputField
							class="mt-1 w-full"
							bind:value={worldName}
							placeholder={m.dedicatedServerDialog_worldPlaceholder()}
						/>
					</div>
					<div>
						<Label>{m.dedicatedServerDialog_password()}</Label><InputField
							class="mt-1 w-full"
							bind:value={gamePassword}
							type="password"
						/>
						<p class="text-primary-500 mt-1 text-sm">{m.dedicatedServerDialog_savedPassword()}</p>
					</div>
					<div class="flex items-center">
						<Label>{m.dedicatedServerDialog_rememberPassword()}</Label><Info
							>{m.dedicatedServerDialog_credentialInfo()}</Info
						><Checkbox bind:checked={rememberGamePassword} />
					</div>
					<div>
						<Label>{m.dedicatedServerDialog_serverPort()}</Label><InputField
							class="mt-1 w-full"
							bind:value={port}
							inputmode="numeric"
						/>
					</div>
					<div class="flex items-center">
						<Label>{m.dedicatedServerDialog_public()}</Label><Info
							>{m.dedicatedServerDialog_publicInfo()}</Info
						><Checkbox bind:checked={publicServer} />
					</div>
					<div class="flex items-center">
						<Label>{m.dedicatedServerDialog_crossplay()}</Label><Info
							>{m.dedicatedServerDialog_crossplayInfo()}</Info
						><Checkbox bind:checked={crossplay} />
					</div>
				</div>
			</Tabs.Content>

			<Tabs.Content value="remote">
				<div class="mt-4 flex flex-col gap-3">
					<InfoBox type={remoteProtocol === 'ftp' ? 'warning' : 'info'}
						>{remoteProtocol === 'sftp'
							? m.dedicatedServerDialog_sftpInfo()
							: remoteProtocol === 'ftps'
								? m.dedicatedServerDialog_ftpsInfo()
								: m.dedicatedServerDialog_ftpInfo()}</InfoBox
					>
					<div>
						<Label>{m.dedicatedServerDialog_protocol()}</Label>
						<Select
							type="single"
							triggerClass="mt-1 w-full"
							bind:value={remoteProtocol}
							onValueChange={(value) => changeRemoteProtocol(value as RemoteProtocol)}
							items={[
								{ value: 'sftp', label: m.dedicatedServerDialog_protocolSftp() },
								{ value: 'ftps', label: m.dedicatedServerDialog_protocolFtps() },
								{ value: 'ftp', label: m.dedicatedServerDialog_protocolFtp() }
							]}
						/>
					</div>
					<div>
						<Label>{m.dedicatedServerDialog_host()}</Label><InputField
							class="mt-1 w-full"
							bind:value={remoteHost}
							placeholder="example.com"
						/>
					</div>
					<div class="grid grid-cols-2 gap-3">
						<div>
							<Label
								>{remoteProtocol === 'sftp'
									? m.dedicatedServerDialog_sshPort()
									: m.dedicatedServerDialog_ftpPort()}</Label
							><InputField class="mt-1 w-full" bind:value={remotePort} inputmode="numeric" />
						</div>
						<div>
							<Label>{m.dedicatedServerDialog_username()}</Label><InputField
								class="mt-1 w-full"
								bind:value={remoteUser}
							/>
						</div>
					</div>
					{#if remoteProtocol === 'sftp'}
						<div>
							<Label>{m.dedicatedServerDialog_authentication()}</Label>
							<Select
								type="single"
								triggerClass="mt-1 w-full"
								bind:value={remoteAuthentication}
								items={[
									{ value: 'password', label: m.dedicatedServerDialog_password() },
									{ value: 'privateKey', label: m.dedicatedServerDialog_privateKeyFile() },
									{ value: 'agent', label: m.dedicatedServerDialog_sshAgent() }
								]}
							/>
						</div>
					{/if}
					{#if remoteProtocol === 'sftp' && remoteAuthentication === 'privateKey'}
						<PathField
							label={m.dedicatedServerDialog_privateKey()}
							bind:value={privateKeyPath}
							onclick={choosePrivateKey}
							icon="mdi:file-key"
						>
							{m.dedicatedServerDialog_privateKeyInfo()}
						</PathField>
					{/if}
					{#if remoteProtocol !== 'sftp' || remoteAuthentication !== 'agent'}
						<div>
							<Label
								>{remoteProtocol !== 'sftp' || remoteAuthentication === 'password'
									? m.dedicatedServerDialog_password()
									: m.dedicatedServerDialog_keyPassphrase()}</Label
							>
							<InputField class="mt-1 w-full" bind:value={remotePassword} type="password" />
							<p class="text-primary-500 mt-1 text-sm">
								{remoteProtocol !== 'sftp' || remoteAuthentication === 'password'
									? m.dedicatedServerDialog_savedPassword()
									: m.dedicatedServerDialog_remoteSavedPassphrase()}
							</p>
						</div>
					{/if}
					<div>
						<Label>{m.dedicatedServerDialog_directory()}</Label><InputField
							class="mt-1 w-full"
							bind:value={remotePath}
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

					<div class="border-primary-300 dark:border-primary-600 mt-2 border-t pt-3">
						<Label>{m.dedicatedServerDialog_syncMode()}</Label>
						<Select
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
									onclick={provisionWorker}>{m.dedicatedServerDialog_localWorkerProvision()}</Button
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
									<InfoBox type="warning">{m.dedicatedServerDialog_localWorkerIncomplete()}</InfoBox
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
							<Label>{m.dedicatedServerDialog_workerAddress()}</Label><InputField
								class="mt-1 w-full"
								bind:value={workerAddress}
								placeholder="http://192.168.1.10:8472"
							/>
						</div>
						<div>
							<Label>{m.dedicatedServerDialog_workerToken()}</Label><InputField
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
							<Label>{m.dedicatedServerDialog_workerAutoSync()}</Label><Info
								>{m.dedicatedServerDialog_workerAutoSyncInfo()}</Info
							><Checkbox bind:checked={workerAutoSync} />
						</div>
						<div class="flex items-center">
							<Label>{m.dedicatedServerDialog_workerAutoMods()}</Label><Info
								>{m.dedicatedServerDialog_workerAutoModsInfo()}</Info
							><Checkbox bind:checked={workerAutoMods} />
						</div>
					{/if}

					<div>
						<Label>{m.dedicatedServerDialog_hostProvider()}</Label>
						<Select
							type="single"
							triggerClass="mt-1 w-full"
							bind:value={hostProvider}
							items={[
								{ value: 'none', label: m.dedicatedServerDialog_hostProviderNone() },
								{ value: 'datHost', label: 'DatHost' }
							]}
						/>
						<p class="text-primary-500 mt-1 text-sm">
							{m.dedicatedServerDialog_hostProviderInfo()}
						</p>
					</div>
					{#if hostProvider === 'datHost'}
						<div>
							<Label>{m.dedicatedServerDialog_datHostServerId()}</Label><InputField
								class="mt-1 w-full"
								bind:value={datHostServerId}
							/>
						</div>
						<div>
							<Label>{m.dedicatedServerDialog_datHostUsername()}</Label><InputField
								class="mt-1 w-full"
								bind:value={datHostUsername}
								placeholder="you@example.com"
							/>
						</div>
						<div>
							<Label>{m.dedicatedServerDialog_datHostPassword()}</Label><InputField
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
						<Label>{m.dedicatedServerDialog_restartPolicy()}</Label>
						<Select
							type="single"
							triggerClass="mt-1 w-full"
							bind:value={restartPolicy}
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
						<Label>{m.dedicatedServerDialog_rememberPassword()}</Label>
						<Info>{m.dedicatedServerDialog_credentialInfo()}</Info>
						<Checkbox bind:checked={rememberRemotePassword} />
					</div>
				</div>
			</Tabs.Content>
		</TabsMenu>

		<details class="mt-4">
			<summary class="text-primary-600 dark:text-primary-300 cursor-pointer"
				>{m.dedicatedServerDialog_advancedOptions()}</summary
			>
			<div class="mt-2">
				<Label>{m.dedicatedServerDialog_additionalArgs()}</Label><InputField
					class="mt-1 w-full"
					bind:value={extraArgs}
					placeholder="-savedir ..."
				/>
			</div>
		</details>
	{/if}

	<div class="mt-5 flex w-full items-center justify-end gap-2">
		<Button color="primary" onclick={() => (open = false)}
			>{m.dedicatedServerDialog_cancel()}</Button
		>
		<Button color="primary" icon="mdi:content-save" loading={saving} onclick={save}
			>{m.dedicatedServerDialog_save()}</Button
		>
		{#if location === 'local'}
			<Button icon="mdi:server" loading={launching} onclick={launch}
				>{m.dedicatedServerDialog_launch()}</Button
			>
		{:else}
			<Button icon="mdi:sync" onclick={syncServer}>{m.dedicatedServerDialog_sync()}</Button>
		{/if}
	</div>
</Dialog>

<ServerSyncDialog bind:open={syncDialogOpen} />
