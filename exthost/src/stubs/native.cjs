// Stub for native / Electron-only / unused optional modules. Any property access
// yields a function that throws loudly when actually invoked, so unimplemented
// native features fail with a clear message instead of silently.
'use strict';
function makeStub(name) {
  const fn = function () {
    throw new Error('[ide-exthost] native module stub "' + name + '" was invoked (not available in the shared extension host)');
  };
  return new Proxy(fn, {
    get(_t, prop) {
      if (prop === '__esModule') { return true; }
      if (prop === 'default') { return stub; }
      if (prop === Symbol.toPrimitive || prop === Symbol.toStringTag) { return undefined; }
      return makeStub(name + '.' + String(prop));
    },
    apply() {
      throw new Error('[ide-exthost] native module stub "' + name + '" was invoked');
    }
  });
}
const stub = makeStub('native');
module.exports = stub;
