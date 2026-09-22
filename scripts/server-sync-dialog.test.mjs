import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { parse } from 'svelte/compiler';

const source = readFileSync(
	new URL('../src/lib/components/dialogs/ServerSyncDialog.svelte', import.meta.url),
	'utf8'
);

// Guard every editable field, including controls in conditional worker/config views.
// These shared components forward disabled to their native or Bits UI controls.
test('server sync editable fields are locked during active operations', () => {
	const fields = [];
	function visit(node) {
		if (!node || typeof node !== 'object') return;
		if (node.type === 'Component' && ['Select', 'Checkbox', 'InputField'].includes(node.name)) {
			fields.push(node);
		}
		for (const value of Object.values(node)) {
			if (Array.isArray(value)) value.forEach(visit);
			else if (value && typeof value === 'object') visit(value);
		}
	}
	visit(parse(source, { modern: true }).fragment);
	assert.ok(fields.length > 0, 'the dialog must contain editable fields');
	const unlocked = fields.filter((field) => {
		const disabled = field.attributes.find((attribute) => attribute.name === 'disabled');
		return disabled?.value?.expression?.name !== 'busy';
	});
	assert.deepEqual(
		unlocked.map(
			(field) => `${field.name} at line ${source.slice(0, field.start).split('\n').length}`
		),
		[],
		'editable fields must use disabled={busy}'
	);
});
