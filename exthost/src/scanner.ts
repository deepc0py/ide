// Scans extension directories into VS Code `IExtensionDescription`s and computes
// activation events (explicit + implicit from contribution points). The implicit
// generators mirror VS Code's own extension-point `activationEventsGenerator`s;
// the real ones live in workbench contribution files we don't bundle.
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { URI, ExtensionIdentifier } from './vs.js';
import type { IExtensionDescription } from './vs.js';

type Contrib = Record<string, unknown>;
type Generator = (contribArr: unknown[]) => string[];

function asArray<T>(value: unknown): T[] {
	if (Array.isArray(value)) { return value as T[]; }
	return value === undefined || value === null ? [] : [value as T];
}

// One generator per contribution-point name. Each receives the (array-wrapped)
// contribution value and returns the implicit activation events it implies.
const GENERATORS: Record<string, Generator> = {
	commands(arr) {
		const out: string[] = [];
		for (const group of arr) {
			for (const c of asArray<{ command?: string }>(group)) {
				if (c?.command) { out.push(`onCommand:${c.command}`); }
			}
		}
		return out;
	},
	languages(arr) {
		const out: string[] = [];
		for (const group of arr) {
			for (const l of asArray<{ id?: string; configuration?: string }>(group)) {
				if (l?.id && l.configuration) { out.push(`onLanguage:${l.id}`); }
			}
		}
		return out;
	},
	authentication(arr) {
		const out: string[] = [];
		for (const group of arr) {
			for (const a of asArray<{ id?: string }>(group)) {
				if (a?.id) { out.push(`onAuthenticationRequest:${a.id}`); }
			}
		}
		return out;
	},
	customEditors(arr) {
		const out: string[] = [];
		for (const group of arr) {
			for (const e of asArray<{ viewType?: string }>(group)) {
				if (e?.viewType) { out.push(`onCustomEditor:${e.viewType}`); }
			}
		}
		return out;
	},
	views(arr) {
		const out: string[] = [];
		for (const group of arr) {
			for (const descriptors of Object.values(group as Record<string, unknown>)) {
				for (const v of asArray<{ id?: string }>(descriptors)) {
					if (v?.id) { out.push(`onView:${v.id}`); }
				}
			}
		}
		return out;
	},
	taskDefinitions(arr) {
		const out: string[] = [];
		for (const group of arr) {
			for (const t of asArray<{ type?: string }>(group)) {
				if (t?.type) { out.push(`onTaskType:${t.type}`); }
			}
		}
		return out;
	},
	debuggers(arr) {
		const out: string[] = [];
		for (const group of arr) {
			for (const d of asArray<{ type?: string }>(group)) {
				if (d?.type) {
					out.push(`onDebugResolve:${d.type}`);
					out.push(`onDebugDynamicConfigurations:${d.type}`);
				}
			}
		}
		return out;
	},
	notebooks(arr) {
		const out: string[] = [];
		for (const group of arr) {
			for (const n of asArray<{ type?: string }>(group)) {
				if (n?.type) { out.push(`onNotebook:${n.type}`); }
			}
		}
		return out;
	},
	walkthroughs(arr) {
		const out: string[] = [];
		for (const group of arr) {
			for (const w of asArray<{ id?: string }>(group)) {
				if (w?.id) { out.push(`onWalkthrough:${w.id}`); }
			}
		}
		return out;
	},
};

function computeActivationEvents(manifest: Contrib): string[] {
	const explicit = asArray<string>(manifest.activationEvents).map(
		(e) => (e === 'onUri' ? `onUri:${String(manifest.publisher)}.${String(manifest.name)}` : e),
	);
	const events = new Set<string>(explicit);
	const contributes = manifest.contributes as Contrib | undefined;
	if (contributes) {
		for (const [point, gen] of Object.entries(GENERATORS)) {
			if (point in contributes) {
				for (const ev of gen(asArray(contributes[point]))) { events.add(ev); }
			}
		}
	}
	return [...events];
}

export interface ScannedExtension {
	description: IExtensionDescription;
	activationEvents: string[];
}

export function scanExtension(dir: string): ScannedExtension {
	const manifest = JSON.parse(readFileSync(join(dir, 'package.json'), 'utf8')) as Contrib;
	const publisher = String(manifest.publisher);
	const name = String(manifest.name);
	const activationEvents = computeActivationEvents(manifest);
	const description = {
		...(manifest as object),
		identifier: new ExtensionIdentifier(`${publisher}.${name}`),
		publisherDisplayName: publisher,
		targetPlatform: 'darwin-arm64',
		isBuiltin: false,
		isUserBuiltin: false,
		isUnderDevelopment: false,
		extensionLocation: URI.file(dir),
		preRelease: false,
		activationEvents,
	} as unknown as IExtensionDescription;
	return { description, activationEvents };
}

export function scanExtensions(dirs: string[]): ScannedExtension[] {
	const out: ScannedExtension[] = [];
	for (const dir of dirs) {
		try {
			out.push(scanExtension(dir));
		} catch (err) {
			console.error(`[ide-exthost] failed to scan extension at ${dir}:`, err);
		}
	}
	return out;
}
