// Central re-export hub for the VS Code (MIT) modules we consume from `.vscode-src`.
// Keeping the deep relative paths in one place avoids per-file churn.
export { URI } from '../.vscode-src/src/vs/base/common/uri.js';
export type { UriComponents } from '../.vscode-src/src/vs/base/common/uri.js';
export { VSBuffer } from '../.vscode-src/src/vs/base/common/buffer.js';
export { Emitter, Event } from '../.vscode-src/src/vs/base/common/event.js';
export { CancellationToken, CancellationTokenSource } from '../.vscode-src/src/vs/base/common/cancellation.js';
export { Disposable, DisposableStore, toDisposable } from '../.vscode-src/src/vs/base/common/lifecycle.js';
export type { IDisposable } from '../.vscode-src/src/vs/base/common/lifecycle.js';

export { RPCProtocol } from '../.vscode-src/src/vs/workbench/services/extensions/common/rpcProtocol.js';
export { SerializableObjectWithBuffers } from '../.vscode-src/src/vs/workbench/services/extensions/common/proxyIdentifier.js';
export type { IMessagePassingProtocol } from '../.vscode-src/src/vs/base/parts/ipc/common/ipc.js';

export { MainContext, ExtHostContext } from '../.vscode-src/src/vs/workbench/api/common/extHost.protocol.js';
export { ISuggestDataDtoField, ISuggestResultDtoField } from '../.vscode-src/src/vs/workbench/api/common/extHost.protocol.js';
export { ExtensionHostMain } from '../.vscode-src/src/vs/workbench/api/common/extensionHostMain.js';
export type { IHostUtils } from '../.vscode-src/src/vs/workbench/api/common/extHostExtensionService.js';

export type { IExtensionHostInitData } from '../.vscode-src/src/vs/workbench/services/extensions/common/extensionHostProtocol.js';
export { ExtensionIdentifier } from '../.vscode-src/src/vs/platform/extensions/common/extensions.js';
export { LogLevel } from '../.vscode-src/src/vs/platform/log/common/log.js';
export { UIKind } from '../.vscode-src/src/vs/workbench/services/extensions/common/extensionHostProtocol.js';
export type { IExtensionDescription } from '../.vscode-src/src/vs/platform/extensions/common/extensions.js';

export { MarkerSeverity } from '../.vscode-src/src/vs/platform/markers/common/markers.js';
export { CompletionItemKind, CompletionItemInsertTextRule } from '../.vscode-src/src/vs/editor/common/languages.js';

export { Registry } from '../.vscode-src/src/vs/platform/registry/common/platform.js';
export {
	Extensions as ConfigurationExtensions,
} from '../.vscode-src/src/vs/platform/configuration/common/configurationRegistry.js';
export type { IConfigurationRegistry } from '../.vscode-src/src/vs/platform/configuration/common/configurationRegistry.js';
export { ConfigurationModelParser } from '../.vscode-src/src/vs/platform/configuration/common/configurationModels.js';
export { ImplicitActivationEvents } from '../.vscode-src/src/vs/platform/extensionManagement/common/implicitActivationEvents.js';
