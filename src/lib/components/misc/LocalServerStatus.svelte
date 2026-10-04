<script lang="ts">
	import server from '$lib/state/server.svelte';
	import * as api from '$lib/api';
	import { confirm } from '@tauri-apps/plugin-dialog';
	import { m } from '$lib/paraglide/messages';
	import Button from '../ui/Button.svelte';
	import InfoBox from '../ui/InfoBox.svelte';

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
	<InfoBox type="info">
		<div class="flex flex-wrap items-center gap-3" role="status">
			<p class="min-w-40 grow basis-60">
				{server.status.stopping
					? m.dedicatedServerDialog_stopping()
					: m.dedicatedServerDialog_running()}
			</p>
			<Button color="primary" loading={stopping || server.status.stopping} onclick={stop}>
				{m.dedicatedServerDialog_forceStop()}
			</Button>
		</div>
	</InfoBox>
{/if}
