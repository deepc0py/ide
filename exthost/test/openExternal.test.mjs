import { test } from 'node:test';
import assert from 'node:assert/strict';
import { openExternal } from '../src/openExternal.ts';



test('launches opener with default command and logs', () => {
	const logs = [];
	const log = (m) => logs.push(m);
	const calls = [];
	const fakeSpawn = (cmd, args, opts) => { calls.push([cmd, args, opts]); return { on() {}, unref() {} }; };

	const ret = openExternal('https://claude.ai/login', log, fakeSpawn);

	assert.equal(ret, true);
	assert.equal(calls.length, 1);
	assert.deepEqual(calls[0], ['open', ['https://claude.ai/login'], { stdio: 'ignore', detached: true }]);
	assert.ok(logs.includes('[ide] openExternal https://claude.ai/login'));
});

test('honors IDE_OPENER override', () => {
	const prev = process.env.IDE_OPENER;
	process.env.IDE_OPENER = '/tmp/opener-stub';
	try {
		const calls = [];
		const fakeSpawn = (cmd, args, opts) => { calls.push([cmd, args, opts]); return { on() {}, unref() {} }; };
		openExternal('https://claude.ai/login', () => {}, fakeSpawn);
		assert.equal(calls[0][0], '/tmp/opener-stub');
	} finally {
		if (prev === undefined) delete process.env.IDE_OPENER;
		else process.env.IDE_OPENER = prev;
	}
});

test('returns false and logs failure when spawn throws', () => {
	const logs = [];
	const log = (m) => logs.push(m);
	const fakeSpawn = () => { throw new Error('boom'); };

	const ret = openExternal('https://claude.ai/login', log, fakeSpawn);

	assert.equal(ret, false);
	assert.ok(logs.some((l) => l.startsWith('[ide] openExternal failed:')));
});
