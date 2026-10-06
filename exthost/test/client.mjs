// Minimal Content-Length framed JSON-RPC 2.0 client used by the exthost tests to
// speak to both the control socket and per-workspace LSP sockets.
import net from 'node:net';

export function connect(socketPath) {
	const socket = net.connect(socketPath);
	let buffer = Buffer.alloc(0);
	let nextId = 0;
	const pending = new Map();
	const notificationWaiters = [];
	const notifications = [];
	const serverRequestHandlers = new Map();

	function deliver(msg) {
		if (msg.id !== undefined && (msg.result !== undefined || msg.error !== undefined) && pending.has(msg.id)) {
			const { resolve, reject } = pending.get(msg.id);
			pending.delete(msg.id);
			if (msg.error) { reject(new Error(msg.error.message)); } else { resolve(msg.result); }
			return;
		}
		if (msg.method && msg.id !== undefined) {
			// server -> client request
			const handler = serverRequestHandlers.get(msg.method);
			const respond = (result) => send({ jsonrpc: '2.0', id: msg.id, result });
			if (handler) { Promise.resolve(handler(msg.params)).then(respond); } else { respond(null); }
			return;
		}
		if (msg.method) {
			notifications.push(msg);
			for (const w of notificationWaiters.splice(0)) { w(); }
		}
	}

	socket.on('data', (chunk) => {
		buffer = Buffer.concat([buffer, chunk]);
		for (;;) {
			const headerEnd = buffer.indexOf('\r\n\r\n');
			if (headerEnd === -1) { return; }
			const m = /Content-Length:\s*(\d+)/i.exec(buffer.subarray(0, headerEnd).toString('utf8'));
			const bodyStart = headerEnd + 4;
			if (!m) { buffer = buffer.subarray(bodyStart); continue; }
			const len = Number(m[1]);
			if (buffer.length < bodyStart + len) { return; }
			const body = buffer.subarray(bodyStart, bodyStart + len).toString('utf8');
			buffer = buffer.subarray(bodyStart + len);
			try { deliver(JSON.parse(body)); } catch { /* ignore */ }
		}
	});

	function send(obj) {
		const payload = Buffer.from(JSON.stringify(obj), 'utf8');
		socket.write(`Content-Length: ${payload.length}\r\n\r\n`);
		socket.write(payload);
	}

	const api = {
		socket,
		ready() {
			const { promise, resolve, reject } = Promise.withResolvers();
			socket.once('connect', resolve);
			socket.once('error', reject);
			return promise;
		},
		request(method, params) {
			const id = ++nextId;
			const { promise, resolve, reject } = Promise.withResolvers();
			pending.set(id, { resolve, reject });
			send({ jsonrpc: '2.0', id, method, params });
			return promise;
		},
		notify(method, params) { send({ jsonrpc: '2.0', method, params }); },
		onServerRequest(method, handler) { serverRequestHandlers.set(method, handler); },
		takeNotifications(predicate) { return notifications.filter(predicate); },
		async waitForNotification(predicate, timeoutMs = 15000) {
			const deadline = Date.now() + timeoutMs;
			for (;;) {
				const hit = notifications.find(predicate);
				if (hit) { return hit; }
				if (Date.now() > deadline) { return undefined; }
				const { promise, resolve } = Promise.withResolvers();
				notificationWaiters.push(resolve);
				const timer = setTimeout(resolve, 250);
				await promise;
				clearTimeout(timer);
			}
		},
		close() { socket.destroy(); },
	};
	return api;
}

export function sleep(ms) {
	const { promise, resolve } = Promise.withResolvers();
	setTimeout(resolve, ms);
	return promise;
}
