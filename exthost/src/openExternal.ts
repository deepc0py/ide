import { spawn as nodeSpawn } from 'node:child_process';
export type SpawnFn = typeof nodeSpawn;
/** Request the OS default browser (via `open`, overridable with IDE_OPENER)
 *  to open `uriString`. Detached, fire-and-forget; never blocks, never
 *  completes login. Returns whether the opener was launched. */
export function openExternal(uriString: string, log: (m: string) => void, spawnFn: SpawnFn = nodeSpawn): boolean {
	const opener = process.env.IDE_OPENER || 'open';
	log(`[ide] openExternal ${uriString}`);
	try {
		const child = spawnFn(opener, [uriString], { stdio: 'ignore', detached: true });
		child?.on?.('error', (e: unknown) => log(`[ide] openExternal error: ${String(e)}`));
		child?.unref?.();
		return true;
	} catch (e) {
		log(`[ide] openExternal failed: ${String(e)}`);
		return false;
	}
}
