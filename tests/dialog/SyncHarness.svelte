<script lang="ts">
	import { Tooltip } from 'bits-ui';
	import Syncer from '$lib/components/toolbar/Syncer.svelte';
	import Toasts from '$lib/components/misc/Toasts.svelte';
</script>

<Tooltip.Provider>
	<Syncer />
	<Toasts />
</Tooltip.Provider>
