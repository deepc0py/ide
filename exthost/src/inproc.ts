// In-memory message-passing protocol pair that connects two RPCProtocol
// instances living in the same worker thread (the real VS Code extension-host
// side and our main-thread shim). Messages are delivered asynchronously via
// microtasks to mirror the async semantics of the real socket transport and to
// avoid re-entrant dispatch inside RPCProtocol.
import { Emitter, VSBuffer } from './vs.js';
import type { IMessagePassingProtocol } from './vs.js';

class InProcEndpoint implements IMessagePassingProtocol {
	private readonly _onMessage = new Emitter<VSBuffer>();
	readonly onMessage = this._onMessage.event;
	other!: InProcEndpoint;

	send(buffer: VSBuffer): void {
		// Copy so that neither side can observe later mutation of the backing store.
		const copy = VSBuffer.wrap(buffer.buffer.slice(0));
		queueMicrotask(() => this.other._onMessage.fire(copy));
	}

	drain(): Promise<void> {
		return Promise.resolve();
	}
}

export function createInProcProtocolPair(): { a: IMessagePassingProtocol; b: IMessagePassingProtocol } {
	const a = new InProcEndpoint();
	const b = new InProcEndpoint();
	a.other = b;
	b.other = a;
	return { a, b };
}
