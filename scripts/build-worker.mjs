// Builds `gale-worker` for the host target and stages it under
// `src-tauri/binaries/`, alongside a WiX fragment that installs it next
// to `gale.exe`. Runs as part of `beforeBuildCommand` (release) and
// `beforeDevCommand` (debug, `--dev`) so `tauri dev` has the helper the
// managed-worker provisioning flow needs.
//
// `externalBin` cannot carry the worker: tauri-build validates and copies
// external binaries on every cargo invocation, which would make a clean
// checkout unbuildable (the worker binary is itself a cargo artifact).
// The generated fragment is only read by the MSI bundler.

import { execSync } from 'node:child_process';
import { copyFileSync, mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const tauriDir = fileURLToPath(new URL('../src-tauri', import.meta.url));
const dev = process.argv.includes('--dev');
const cargoArgs = dev ? [] : ['--release'];
const profile = dev ? 'debug' : 'release';

execSync(['cargo', 'build', ...cargoArgs, '--features', 'worker', '--bin', 'gale-worker'].join(' '), {
	cwd: tauriDir,
	stdio: 'inherit',
});

const isWindows = process.platform === 'win32';
const exe = isWindows ? 'gale-worker.exe' : 'gale-worker';
const outDir = join(tauriDir, 'binaries');
mkdirSync(outDir, { recursive: true });
const staged = join(outDir, exe);
copyFileSync(join(tauriDir, 'target', profile, exe), staged);

// Referenced by `bundle.windows.wix.fragmentPaths` — the ComponentGroup
// id is wired into the MSI's External feature via `componentGroupRefs`.
// In dev the staged binary only feeds `worker_exe()`'s sibling lookup.
writeFileSync(
	join(outDir, 'worker.wxs'),
	`<Wix xmlns="http://schemas.microsoft.com/wix/2006/wi">
	<Fragment>
		<ComponentGroup Id="GaleWorkerBinaries">
			<Component Id="GaleWorkerBinary" Guid="*" Win64="yes" Directory="INSTALLDIR">
				<File Id="GaleWorkerExe" Source="${staged}" KeyPath="yes" />
			</Component>
		</ComponentGroup>
	</Fragment>
</Wix>
`,
);

console.log(`staged ${staged}`);
