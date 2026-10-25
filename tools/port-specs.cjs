// Ports cordis@4.0.0-rc.8's vitest specs (TypeScript) to plain ES modules for Verve.
// Usage: node port-specs.cjs <upstream packages/core/tests> <out dir>
const fs = require('fs');
const path = require('path');
const { stripTypeScriptTypes } = require('module');

const [src, out] = process.argv.slice(2);
fs.mkdirSync(out, { recursive: true });
for (const name of fs.readdirSync(src)) {
  if (!name.endsWith('.ts')) continue;
  let code = fs.readFileSync(path.join(src, name), 'utf8');
  // `declare module` blocks are types only; strip-types refuses them, so drop them first.
  code = code.replace(/^declare module [^{]+\{[\s\S]*?^\}\n?/gm, '');
  if (name === 'decorator.spec.ts') {
    // Boa has no decorator syntax: apply `@Inject('foo')` by hand, as the TC39 proposal
    // would, through a method decorator context with an initializer list.
    code = code.replace(
      /      @Inject\('foo'\)\n      method\(\) \{/,
      '      method() {',
    );
    code = code.replace(
      /    const root = new Context\(\)\n    await root\.plugin\(Bar\)/,
      [
        '    {',
        "      const initializers = []",
        "      const decorated = Inject('foo')(Bar.prototype.method, { kind: 'method', name: 'method', addInitializer: (fn) => initializers.push(fn) })",
        '      if (decorated) Bar.prototype.method = decorated',
        '      const Original = Bar',
        '      Bar = class extends Original {',
        '        constructor(...args) {',
        '          super(...args)',
        '          for (const fn of initializers) fn.call(this)',
        '        }',
        '      }',
        '    }',
        '    const root = new Context()',
        '    await root.plugin(Bar)',
      ].join('\n'),
    );
    code = code.replace('    class Bar extends Service {', '    let Bar = class Bar extends Service {');
  }
  let js = stripTypeScriptTypes(code, { mode: 'transform', sourceMap: false });
  js = js
    .replace(/from '\.\.\/src'/g, "from 'cordis'")
    .replace(/from '\.\/utils'/g, "from './utils.mjs'");
  // Type-only imports leave empty bindings behind; cosmokit is not needed at runtime here.
  js = js.replace(/^import \{[^}]*\} from 'cosmokit';?\n/gm, '');
  // Type-only imports leave bindings behind that name no value; drop them.
  js = js.replace(/^import \{[^}]*\} from '\.\.\/src\/logger';?\n/gm, '');
  js = js.replace("import { Context, Events } from 'cordis';", "import { Context } from 'cordis';");
  const header = `// Ported from cordis@4.0.0-rc.8 packages/core/tests/${name} (MIT, cordiverse/cordis).\n// Types stripped by tools/port-specs.cjs; behaviour unchanged.\n`;
  fs.writeFileSync(path.join(out, name.replace(/\.ts$/, '.mjs')), header + js);
  console.log('ported', name);
}
