// Builds the `IConfigurationData` the ExtHost configuration service consumes.
// We register every extension's `contributes.configuration` into VS Code's
// ConfigurationRegistry (so contributed defaults & scopes exist), then project
// the registry defaults plus the user's settings.json into the init payload.
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { Registry, ConfigurationExtensions, ConfigurationModelParser, URI } from './vs.js';
import { NullLogService } from '../.vscode-src/src/vs/platform/log/common/log.js';
import type { IConfigurationRegistry, IConfigurationNode } from '../.vscode-src/src/vs/platform/configuration/common/configurationRegistry.js';
import type { IConfigurationModel, IConfigurationData } from '../.vscode-src/src/vs/platform/configuration/common/configuration.js';
import type { ScannedExtension } from './scanner.js';

const logService = new NullLogService();

function emptyModel(): IConfigurationModel {
	return { contents: {}, keys: [], overrides: [] };
}

function rawToModel(raw: Record<string, unknown>): IConfigurationModel {
	const parser = new ConfigurationModelParser('ide', logService);
	parser.parseRaw(raw);
	const model = parser.configurationModel;
	return { contents: model.contents, keys: model.keys, overrides: model.overrides };
}

function registerExtensionConfigurations(extensions: ScannedExtension[]): void {
	const registry = Registry.as<IConfigurationRegistry>(ConfigurationExtensions.Configuration);
	const nodes: IConfigurationNode[] = [];
	for (const { description } of extensions) {
		const contributed = description.contributes?.configuration;
		if (!contributed) { continue; }
		for (const node of Array.isArray(contributed) ? contributed : [contributed]) {
			nodes.push(node as IConfigurationNode);
		}
	}
	if (nodes.length > 0) {
		registry.registerConfigurations(nodes, false);
	}
}

function defaultsModel(): IConfigurationModel {
	const registry = Registry.as<IConfigurationRegistry>(ConfigurationExtensions.Configuration);
	const properties = registry.getConfigurationProperties();
	const raw: Record<string, unknown> = {};
	for (const key of Object.keys(properties)) {
		const schema = properties[key];
		if (schema && schema.default !== undefined) {
			raw[key] = schema.default;
		}
	}
	return rawToModel(raw);
}

function configurationScopes(): [string, number | undefined][] {
	const registry = Registry.as<IConfigurationRegistry>(ConfigurationExtensions.Configuration);
	const properties = registry.getConfigurationProperties();
	const scopes: [string, number | undefined][] = [];
	for (const key of Object.keys(properties)) {
		scopes.push([key, properties[key].scope]);
	}
	return scopes;
}

export interface ConfigurationInitData extends IConfigurationData {
	configurationScopes: [string, number | undefined][];
}

export function buildConfiguration(extensions: ScannedExtension[], dataDir: string, folders: string[]): ConfigurationInitData {
	registerExtensionConfigurations(extensions);
	const userParser = new ConfigurationModelParser('user', logService);
	try {
		userParser.parse(readFileSync(join(dataDir, 'User', 'settings.json'), 'utf8'));
	} catch {
		// no user settings file -> empty user model
	}
	const userModel = userParser.configurationModel;
	return {
		defaults: defaultsModel(),
		policy: emptyModel(),
		application: emptyModel(),
		userLocal: { contents: userModel.contents, keys: userModel.keys, overrides: userModel.overrides },
		userRemote: emptyModel(),
		workspace: emptyModel(),
		folders: folders.map((f) => [URI.file(f), emptyModel()]),
		configurationScopes: configurationScopes(),
	};
}
