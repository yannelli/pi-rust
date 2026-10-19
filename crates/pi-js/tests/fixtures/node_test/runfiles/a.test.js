const { test, describe, it } = require('node:test');
test('a1', () => {});
describe('ad', () => { it('a2', () => { throw new Error('a2 fails'); }); });
