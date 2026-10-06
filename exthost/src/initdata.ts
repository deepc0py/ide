// Builds the `IExtensionHostInitData` that the real VS Code `ExtensionHostMain`
// consumes. Only the fields the node extension host actually reads are populated
// with meaningful values; the rest use benign defaults.
import { randomUUID } from 'node:crypto';
import { URI, LogLevel, UIKind, ExtensionIdentifier } from './vs.js';
import type { IExtensionHostInitData } from './vs.js';
import type { ScannedExtension } from './scanner.js';

export interface InitDataOptions {
	dataDir: string;
	logsDir: string;
	appRoot: string;
	version: string;
	extensions: ScannedExtension[];
}

export function buildInitData(opts: InitDataOptions): IExtensionHostInitData {
	const allExtensions = opts.extensions.map((e) => e.description);
	const activationEvents: Record<string, string[]> = Object.create(null);
	const myExtensions: ExtensionIdentifier[] = [];
	for (const ext of opts.extensions) {
		myExtensions.push(ext.description.identifier);
		if (ext.activationEvents.length > 0) {
			activationEvents[ExtensionIdentifier.toKey(ext.description.identifier)] = ext.activationEvents;
		}
	}

	return {
		version: opts.version,
		quality: 'stable',
		parentPid: 0,
		environment: {
			isExtensionDevelopmentDebug: false,
			appName: 'ide',
			appHost: 'ide',
			appRoot: URI.file(opts.appRoot),
			appLanguage: 'en',
			isExtensionTelemetryLoggingOnly: false,
			appUriScheme: 'ide',
			globalStorageHome: URI.file(`${opts.dataDir}/User/globalStorage`),
			workspaceStorageHome: URI.file(`${opts.dataDir}/User/workspaceStorage`),
			useHostProxy: false,
			skipWorkspaceStorageLock: true,
		},
		extensions: {
			versionId: 1,
			allExtensions,
			activationEvents,
			myExtensions,
		},
		telemetryInfo: {
			sessionId: randomUUID(),
			machineId: 'ide-machine',
			sqmId: '',
			devDeviceId: 'ide-device',
			firstSessionDate: new Date().toUTCString(),
		},
		logLevel: LogLevel.Info,
		loggers: [],
		logsLocation: URI.file(opts.logsDir),
		autoStart: true,
		remote: { isRemote: false, authority: undefined, connectionData: null },
		consoleForward: { includeStack: false, logNative: true },
		uiKind: UIKind.Desktop,
	};
}
