// @bun
var __create = Object.create;
var __getProtoOf = Object.getPrototypeOf;
var __defProp = Object.defineProperty;
var __getOwnPropNames = Object.getOwnPropertyNames;
var __hasOwnProp = Object.prototype.hasOwnProperty;
function __accessProp(key) {
  return this[key];
}
var __toESMCache_node;
var __toESMCache_esm;
var __toESM = (mod, isNodeMode, target) => {
  var canCache = mod != null && typeof mod === "object";
  if (canCache) {
    var cache = isNodeMode ? __toESMCache_node ??= new WeakMap : __toESMCache_esm ??= new WeakMap;
    var cached = cache.get(mod);
    if (cached)
      return cached;
  }
  target = mod != null ? __create(__getProtoOf(mod)) : {};
  const to = isNodeMode || !mod || !mod.__esModule ? __defProp(target, "default", { value: mod, enumerable: true }) : target;
  if (mod && typeof mod === "object" || typeof mod === "function") {
    for (let key of __getOwnPropNames(mod))
      if (!__hasOwnProp.call(to, key))
        __defProp(to, key, {
          get: __accessProp.bind(mod, key),
          enumerable: true
        });
  }
  if (canCache)
    cache.set(mod, to);
  return to;
};
var __commonJS = (cb, mod) => () => (mod || cb((mod = { exports: {} }).exports, mod), mod.exports);

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/compile/codegen/code.js
var require_code = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.regexpCode = exports.getEsmExportName = exports.getProperty = exports.safeStringify = exports.stringify = exports.strConcat = exports.addCodeArg = exports.str = exports._ = exports.nil = exports._Code = exports.Name = exports.IDENTIFIER = exports._CodeOrName = undefined;

  class _CodeOrName {
  }
  exports._CodeOrName = _CodeOrName;
  exports.IDENTIFIER = /^[a-z$_][a-z$_0-9]*$/i;

  class Name extends _CodeOrName {
    constructor(s) {
      super();
      if (!exports.IDENTIFIER.test(s))
        throw new Error("CodeGen: name must be a valid identifier");
      this.str = s;
    }
    toString() {
      return this.str;
    }
    emptyStr() {
      return false;
    }
    get names() {
      return { [this.str]: 1 };
    }
  }
  exports.Name = Name;

  class _Code extends _CodeOrName {
    constructor(code) {
      super();
      this._items = typeof code === "string" ? [code] : code;
    }
    toString() {
      return this.str;
    }
    emptyStr() {
      if (this._items.length > 1)
        return false;
      const item = this._items[0];
      return item === "" || item === '""';
    }
    get str() {
      var _a;
      return (_a = this._str) !== null && _a !== undefined ? _a : this._str = this._items.reduce((s, c) => `${s}${c}`, "");
    }
    get names() {
      var _a;
      return (_a = this._names) !== null && _a !== undefined ? _a : this._names = this._items.reduce((names, c) => {
        if (c instanceof Name)
          names[c.str] = (names[c.str] || 0) + 1;
        return names;
      }, {});
    }
  }
  exports._Code = _Code;
  exports.nil = new _Code("");
  function _(strs, ...args) {
    const code = [strs[0]];
    let i = 0;
    while (i < args.length) {
      addCodeArg(code, args[i]);
      code.push(strs[++i]);
    }
    return new _Code(code);
  }
  exports._ = _;
  var plus = new _Code("+");
  function str(strs, ...args) {
    const expr = [safeStringify(strs[0])];
    let i = 0;
    while (i < args.length) {
      expr.push(plus);
      addCodeArg(expr, args[i]);
      expr.push(plus, safeStringify(strs[++i]));
    }
    optimize(expr);
    return new _Code(expr);
  }
  exports.str = str;
  function addCodeArg(code, arg) {
    if (arg instanceof _Code)
      code.push(...arg._items);
    else if (arg instanceof Name)
      code.push(arg);
    else
      code.push(interpolate(arg));
  }
  exports.addCodeArg = addCodeArg;
  function optimize(expr) {
    let i = 1;
    while (i < expr.length - 1) {
      if (expr[i] === plus) {
        const res = mergeExprItems(expr[i - 1], expr[i + 1]);
        if (res !== undefined) {
          expr.splice(i - 1, 3, res);
          continue;
        }
        expr[i++] = "+";
      }
      i++;
    }
  }
  function mergeExprItems(a, b) {
    if (b === '""')
      return a;
    if (a === '""')
      return b;
    if (typeof a == "string") {
      if (b instanceof Name || a[a.length - 1] !== '"')
        return;
      if (typeof b != "string")
        return `${a.slice(0, -1)}${b}"`;
      if (b[0] === '"')
        return a.slice(0, -1) + b.slice(1);
      return;
    }
    if (typeof b == "string" && b[0] === '"' && !(a instanceof Name))
      return `"${a}${b.slice(1)}`;
    return;
  }
  function strConcat(c1, c2) {
    return c2.emptyStr() ? c1 : c1.emptyStr() ? c2 : str`${c1}${c2}`;
  }
  exports.strConcat = strConcat;
  function interpolate(x) {
    return typeof x == "number" || typeof x == "boolean" || x === null ? x : safeStringify(Array.isArray(x) ? x.join(",") : x);
  }
  function stringify(x) {
    return new _Code(safeStringify(x));
  }
  exports.stringify = stringify;
  function safeStringify(x) {
    return JSON.stringify(x).replace(/\u2028/g, "\\u2028").replace(/\u2029/g, "\\u2029");
  }
  exports.safeStringify = safeStringify;
  function getProperty(key) {
    return typeof key == "string" && exports.IDENTIFIER.test(key) ? new _Code(`.${key}`) : _`[${key}]`;
  }
  exports.getProperty = getProperty;
  function getEsmExportName(key) {
    if (typeof key == "string" && exports.IDENTIFIER.test(key)) {
      return new _Code(`${key}`);
    }
    throw new Error(`CodeGen: invalid export name: ${key}, use explicit $id name mapping`);
  }
  exports.getEsmExportName = getEsmExportName;
  function regexpCode(rx) {
    return new _Code(rx.toString());
  }
  exports.regexpCode = regexpCode;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/compile/codegen/scope.js
var require_scope = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.ValueScope = exports.ValueScopeName = exports.Scope = exports.varKinds = exports.UsedValueState = undefined;
  var code_1 = require_code();

  class ValueError extends Error {
    constructor(name) {
      super(`CodeGen: "code" for ${name} not defined`);
      this.value = name.value;
    }
  }
  var UsedValueState;
  (function(UsedValueState2) {
    UsedValueState2[UsedValueState2["Started"] = 0] = "Started";
    UsedValueState2[UsedValueState2["Completed"] = 1] = "Completed";
  })(UsedValueState || (exports.UsedValueState = UsedValueState = {}));
  exports.varKinds = {
    const: new code_1.Name("const"),
    let: new code_1.Name("let"),
    var: new code_1.Name("var")
  };

  class Scope {
    constructor({ prefixes, parent } = {}) {
      this._names = {};
      this._prefixes = prefixes;
      this._parent = parent;
    }
    toName(nameOrPrefix) {
      return nameOrPrefix instanceof code_1.Name ? nameOrPrefix : this.name(nameOrPrefix);
    }
    name(prefix) {
      return new code_1.Name(this._newName(prefix));
    }
    _newName(prefix) {
      const ng = this._names[prefix] || this._nameGroup(prefix);
      return `${prefix}${ng.index++}`;
    }
    _nameGroup(prefix) {
      var _a, _b;
      if (((_b = (_a = this._parent) === null || _a === undefined ? undefined : _a._prefixes) === null || _b === undefined ? undefined : _b.has(prefix)) || this._prefixes && !this._prefixes.has(prefix)) {
        throw new Error(`CodeGen: prefix "${prefix}" is not allowed in this scope`);
      }
      return this._names[prefix] = { prefix, index: 0 };
    }
  }
  exports.Scope = Scope;

  class ValueScopeName extends code_1.Name {
    constructor(prefix, nameStr) {
      super(nameStr);
      this.prefix = prefix;
    }
    setValue(value, { property, itemIndex }) {
      this.value = value;
      this.scopePath = (0, code_1._)`.${new code_1.Name(property)}[${itemIndex}]`;
    }
  }
  exports.ValueScopeName = ValueScopeName;
  var line = (0, code_1._)`\n`;

  class ValueScope extends Scope {
    constructor(opts) {
      super(opts);
      this._values = {};
      this._scope = opts.scope;
      this.opts = { ...opts, _n: opts.lines ? line : code_1.nil };
    }
    get() {
      return this._scope;
    }
    name(prefix) {
      return new ValueScopeName(prefix, this._newName(prefix));
    }
    value(nameOrPrefix, value) {
      var _a;
      if (value.ref === undefined)
        throw new Error("CodeGen: ref must be passed in value");
      const name = this.toName(nameOrPrefix);
      const { prefix } = name;
      const valueKey = (_a = value.key) !== null && _a !== undefined ? _a : value.ref;
      let vs = this._values[prefix];
      if (vs) {
        const _name = vs.get(valueKey);
        if (_name)
          return _name;
      } else {
        vs = this._values[prefix] = new Map;
      }
      vs.set(valueKey, name);
      const s = this._scope[prefix] || (this._scope[prefix] = []);
      const itemIndex = s.length;
      s[itemIndex] = value.ref;
      name.setValue(value, { property: prefix, itemIndex });
      return name;
    }
    getValue(prefix, keyOrRef) {
      const vs = this._values[prefix];
      if (!vs)
        return;
      return vs.get(keyOrRef);
    }
    scopeRefs(scopeName, values = this._values) {
      return this._reduceValues(values, (name) => {
        if (name.scopePath === undefined)
          throw new Error(`CodeGen: name "${name}" has no value`);
        return (0, code_1._)`${scopeName}${name.scopePath}`;
      });
    }
    scopeCode(values = this._values, usedValues, getCode) {
      return this._reduceValues(values, (name) => {
        if (name.value === undefined)
          throw new Error(`CodeGen: name "${name}" has no value`);
        return name.value.code;
      }, usedValues, getCode);
    }
    _reduceValues(values, valueCode, usedValues = {}, getCode) {
      let code = code_1.nil;
      for (const prefix in values) {
        const vs = values[prefix];
        if (!vs)
          continue;
        const nameSet = usedValues[prefix] = usedValues[prefix] || new Map;
        vs.forEach((name) => {
          if (nameSet.has(name))
            return;
          nameSet.set(name, UsedValueState.Started);
          let c = valueCode(name);
          if (c) {
            const def = this.opts.es5 ? exports.varKinds.var : exports.varKinds.const;
            code = (0, code_1._)`${code}${def} ${name} = ${c};${this.opts._n}`;
          } else if (c = getCode === null || getCode === undefined ? undefined : getCode(name)) {
            code = (0, code_1._)`${code}${c}${this.opts._n}`;
          } else {
            throw new ValueError(name);
          }
          nameSet.set(name, UsedValueState.Completed);
        });
      }
      return code;
    }
  }
  exports.ValueScope = ValueScope;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/compile/codegen/index.js
var require_codegen = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.or = exports.and = exports.not = exports.CodeGen = exports.operators = exports.varKinds = exports.ValueScopeName = exports.ValueScope = exports.Scope = exports.Name = exports.regexpCode = exports.stringify = exports.getProperty = exports.nil = exports.strConcat = exports.str = exports._ = undefined;
  var code_1 = require_code();
  var scope_1 = require_scope();
  var code_2 = require_code();
  Object.defineProperty(exports, "_", { enumerable: true, get: function() {
    return code_2._;
  } });
  Object.defineProperty(exports, "str", { enumerable: true, get: function() {
    return code_2.str;
  } });
  Object.defineProperty(exports, "strConcat", { enumerable: true, get: function() {
    return code_2.strConcat;
  } });
  Object.defineProperty(exports, "nil", { enumerable: true, get: function() {
    return code_2.nil;
  } });
  Object.defineProperty(exports, "getProperty", { enumerable: true, get: function() {
    return code_2.getProperty;
  } });
  Object.defineProperty(exports, "stringify", { enumerable: true, get: function() {
    return code_2.stringify;
  } });
  Object.defineProperty(exports, "regexpCode", { enumerable: true, get: function() {
    return code_2.regexpCode;
  } });
  Object.defineProperty(exports, "Name", { enumerable: true, get: function() {
    return code_2.Name;
  } });
  var scope_2 = require_scope();
  Object.defineProperty(exports, "Scope", { enumerable: true, get: function() {
    return scope_2.Scope;
  } });
  Object.defineProperty(exports, "ValueScope", { enumerable: true, get: function() {
    return scope_2.ValueScope;
  } });
  Object.defineProperty(exports, "ValueScopeName", { enumerable: true, get: function() {
    return scope_2.ValueScopeName;
  } });
  Object.defineProperty(exports, "varKinds", { enumerable: true, get: function() {
    return scope_2.varKinds;
  } });
  exports.operators = {
    GT: new code_1._Code(">"),
    GTE: new code_1._Code(">="),
    LT: new code_1._Code("<"),
    LTE: new code_1._Code("<="),
    EQ: new code_1._Code("==="),
    NEQ: new code_1._Code("!=="),
    NOT: new code_1._Code("!"),
    OR: new code_1._Code("||"),
    AND: new code_1._Code("&&"),
    ADD: new code_1._Code("+")
  };

  class Node {
    optimizeNodes() {
      return this;
    }
    optimizeNames(_names, _constants) {
      return this;
    }
  }

  class Def extends Node {
    constructor(varKind, name, rhs) {
      super();
      this.varKind = varKind;
      this.name = name;
      this.rhs = rhs;
    }
    render({ es5, _n }) {
      const varKind = es5 ? scope_1.varKinds.var : this.varKind;
      const rhs = this.rhs === undefined ? "" : ` = ${this.rhs}`;
      return `${varKind} ${this.name}${rhs};` + _n;
    }
    optimizeNames(names, constants) {
      if (!names[this.name.str])
        return;
      if (this.rhs)
        this.rhs = optimizeExpr(this.rhs, names, constants);
      return this;
    }
    get names() {
      return this.rhs instanceof code_1._CodeOrName ? this.rhs.names : {};
    }
  }

  class Assign extends Node {
    constructor(lhs, rhs, sideEffects) {
      super();
      this.lhs = lhs;
      this.rhs = rhs;
      this.sideEffects = sideEffects;
    }
    render({ _n }) {
      return `${this.lhs} = ${this.rhs};` + _n;
    }
    optimizeNames(names, constants) {
      if (this.lhs instanceof code_1.Name && !names[this.lhs.str] && !this.sideEffects)
        return;
      this.rhs = optimizeExpr(this.rhs, names, constants);
      return this;
    }
    get names() {
      const names = this.lhs instanceof code_1.Name ? {} : { ...this.lhs.names };
      return addExprNames(names, this.rhs);
    }
  }

  class AssignOp extends Assign {
    constructor(lhs, op, rhs, sideEffects) {
      super(lhs, rhs, sideEffects);
      this.op = op;
    }
    render({ _n }) {
      return `${this.lhs} ${this.op}= ${this.rhs};` + _n;
    }
  }

  class Label extends Node {
    constructor(label) {
      super();
      this.label = label;
      this.names = {};
    }
    render({ _n }) {
      return `${this.label}:` + _n;
    }
  }

  class Break extends Node {
    constructor(label) {
      super();
      this.label = label;
      this.names = {};
    }
    render({ _n }) {
      const label = this.label ? ` ${this.label}` : "";
      return `break${label};` + _n;
    }
  }

  class Throw extends Node {
    constructor(error) {
      super();
      this.error = error;
    }
    render({ _n }) {
      return `throw ${this.error};` + _n;
    }
    get names() {
      return this.error.names;
    }
  }

  class AnyCode extends Node {
    constructor(code) {
      super();
      this.code = code;
    }
    render({ _n }) {
      return `${this.code};` + _n;
    }
    optimizeNodes() {
      return `${this.code}` ? this : undefined;
    }
    optimizeNames(names, constants) {
      this.code = optimizeExpr(this.code, names, constants);
      return this;
    }
    get names() {
      return this.code instanceof code_1._CodeOrName ? this.code.names : {};
    }
  }

  class ParentNode extends Node {
    constructor(nodes = []) {
      super();
      this.nodes = nodes;
    }
    render(opts) {
      return this.nodes.reduce((code, n) => code + n.render(opts), "");
    }
    optimizeNodes() {
      const { nodes } = this;
      let i = nodes.length;
      while (i--) {
        const n = nodes[i].optimizeNodes();
        if (Array.isArray(n))
          nodes.splice(i, 1, ...n);
        else if (n)
          nodes[i] = n;
        else
          nodes.splice(i, 1);
      }
      return nodes.length > 0 ? this : undefined;
    }
    optimizeNames(names, constants) {
      const { nodes } = this;
      let i = nodes.length;
      while (i--) {
        const n = nodes[i];
        if (n.optimizeNames(names, constants))
          continue;
        subtractNames(names, n.names);
        nodes.splice(i, 1);
      }
      return nodes.length > 0 ? this : undefined;
    }
    get names() {
      return this.nodes.reduce((names, n) => addNames(names, n.names), {});
    }
  }

  class BlockNode extends ParentNode {
    render(opts) {
      return "{" + opts._n + super.render(opts) + "}" + opts._n;
    }
  }

  class Root extends ParentNode {
  }

  class Else extends BlockNode {
  }
  Else.kind = "else";

  class If extends BlockNode {
    constructor(condition, nodes) {
      super(nodes);
      this.condition = condition;
    }
    render(opts) {
      let code = `if(${this.condition})` + super.render(opts);
      if (this.else)
        code += "else " + this.else.render(opts);
      return code;
    }
    optimizeNodes() {
      super.optimizeNodes();
      const cond = this.condition;
      if (cond === true)
        return this.nodes;
      let e = this.else;
      if (e) {
        const ns = e.optimizeNodes();
        e = this.else = Array.isArray(ns) ? new Else(ns) : ns;
      }
      if (e) {
        if (cond === false)
          return e instanceof If ? e : e.nodes;
        if (this.nodes.length)
          return this;
        return new If(not(cond), e instanceof If ? [e] : e.nodes);
      }
      if (cond === false || !this.nodes.length)
        return;
      return this;
    }
    optimizeNames(names, constants) {
      var _a;
      this.else = (_a = this.else) === null || _a === undefined ? undefined : _a.optimizeNames(names, constants);
      if (!(super.optimizeNames(names, constants) || this.else))
        return;
      this.condition = optimizeExpr(this.condition, names, constants);
      return this;
    }
    get names() {
      const names = super.names;
      addExprNames(names, this.condition);
      if (this.else)
        addNames(names, this.else.names);
      return names;
    }
  }
  If.kind = "if";

  class For extends BlockNode {
  }
  For.kind = "for";

  class ForLoop extends For {
    constructor(iteration) {
      super();
      this.iteration = iteration;
    }
    render(opts) {
      return `for(${this.iteration})` + super.render(opts);
    }
    optimizeNames(names, constants) {
      if (!super.optimizeNames(names, constants))
        return;
      this.iteration = optimizeExpr(this.iteration, names, constants);
      return this;
    }
    get names() {
      return addNames(super.names, this.iteration.names);
    }
  }

  class ForRange extends For {
    constructor(varKind, name, from, to) {
      super();
      this.varKind = varKind;
      this.name = name;
      this.from = from;
      this.to = to;
    }
    render(opts) {
      const varKind = opts.es5 ? scope_1.varKinds.var : this.varKind;
      const { name, from, to } = this;
      return `for(${varKind} ${name}=${from}; ${name}<${to}; ${name}++)` + super.render(opts);
    }
    get names() {
      const names = addExprNames(super.names, this.from);
      return addExprNames(names, this.to);
    }
  }

  class ForIter extends For {
    constructor(loop, varKind, name, iterable) {
      super();
      this.loop = loop;
      this.varKind = varKind;
      this.name = name;
      this.iterable = iterable;
    }
    render(opts) {
      return `for(${this.varKind} ${this.name} ${this.loop} ${this.iterable})` + super.render(opts);
    }
    optimizeNames(names, constants) {
      if (!super.optimizeNames(names, constants))
        return;
      this.iterable = optimizeExpr(this.iterable, names, constants);
      return this;
    }
    get names() {
      return addNames(super.names, this.iterable.names);
    }
  }

  class Func extends BlockNode {
    constructor(name, args, async) {
      super();
      this.name = name;
      this.args = args;
      this.async = async;
    }
    render(opts) {
      const _async = this.async ? "async " : "";
      return `${_async}function ${this.name}(${this.args})` + super.render(opts);
    }
  }
  Func.kind = "func";

  class Return extends ParentNode {
    render(opts) {
      return "return " + super.render(opts);
    }
  }
  Return.kind = "return";

  class Try extends BlockNode {
    render(opts) {
      let code = "try" + super.render(opts);
      if (this.catch)
        code += this.catch.render(opts);
      if (this.finally)
        code += this.finally.render(opts);
      return code;
    }
    optimizeNodes() {
      var _a, _b;
      super.optimizeNodes();
      (_a = this.catch) === null || _a === undefined || _a.optimizeNodes();
      (_b = this.finally) === null || _b === undefined || _b.optimizeNodes();
      return this;
    }
    optimizeNames(names, constants) {
      var _a, _b;
      super.optimizeNames(names, constants);
      (_a = this.catch) === null || _a === undefined || _a.optimizeNames(names, constants);
      (_b = this.finally) === null || _b === undefined || _b.optimizeNames(names, constants);
      return this;
    }
    get names() {
      const names = super.names;
      if (this.catch)
        addNames(names, this.catch.names);
      if (this.finally)
        addNames(names, this.finally.names);
      return names;
    }
  }

  class Catch extends BlockNode {
    constructor(error) {
      super();
      this.error = error;
    }
    render(opts) {
      return `catch(${this.error})` + super.render(opts);
    }
  }
  Catch.kind = "catch";

  class Finally extends BlockNode {
    render(opts) {
      return "finally" + super.render(opts);
    }
  }
  Finally.kind = "finally";

  class CodeGen {
    constructor(extScope, opts = {}) {
      this._values = {};
      this._blockStarts = [];
      this._constants = {};
      this.opts = { ...opts, _n: opts.lines ? `
` : "" };
      this._extScope = extScope;
      this._scope = new scope_1.Scope({ parent: extScope });
      this._nodes = [new Root];
    }
    toString() {
      return this._root.render(this.opts);
    }
    name(prefix) {
      return this._scope.name(prefix);
    }
    scopeName(prefix) {
      return this._extScope.name(prefix);
    }
    scopeValue(prefixOrName, value) {
      const name = this._extScope.value(prefixOrName, value);
      const vs = this._values[name.prefix] || (this._values[name.prefix] = new Set);
      vs.add(name);
      return name;
    }
    getScopeValue(prefix, keyOrRef) {
      return this._extScope.getValue(prefix, keyOrRef);
    }
    scopeRefs(scopeName) {
      return this._extScope.scopeRefs(scopeName, this._values);
    }
    scopeCode() {
      return this._extScope.scopeCode(this._values);
    }
    _def(varKind, nameOrPrefix, rhs, constant) {
      const name = this._scope.toName(nameOrPrefix);
      if (rhs !== undefined && constant)
        this._constants[name.str] = rhs;
      this._leafNode(new Def(varKind, name, rhs));
      return name;
    }
    const(nameOrPrefix, rhs, _constant) {
      return this._def(scope_1.varKinds.const, nameOrPrefix, rhs, _constant);
    }
    let(nameOrPrefix, rhs, _constant) {
      return this._def(scope_1.varKinds.let, nameOrPrefix, rhs, _constant);
    }
    var(nameOrPrefix, rhs, _constant) {
      return this._def(scope_1.varKinds.var, nameOrPrefix, rhs, _constant);
    }
    assign(lhs, rhs, sideEffects) {
      return this._leafNode(new Assign(lhs, rhs, sideEffects));
    }
    add(lhs, rhs) {
      return this._leafNode(new AssignOp(lhs, exports.operators.ADD, rhs));
    }
    code(c) {
      if (typeof c == "function")
        c();
      else if (c !== code_1.nil)
        this._leafNode(new AnyCode(c));
      return this;
    }
    object(...keyValues) {
      const code = ["{"];
      for (const [key, value] of keyValues) {
        if (code.length > 1)
          code.push(",");
        code.push(key);
        if (key !== value || this.opts.es5) {
          code.push(":");
          (0, code_1.addCodeArg)(code, value);
        }
      }
      code.push("}");
      return new code_1._Code(code);
    }
    if(condition, thenBody, elseBody) {
      this._blockNode(new If(condition));
      if (thenBody && elseBody) {
        this.code(thenBody).else().code(elseBody).endIf();
      } else if (thenBody) {
        this.code(thenBody).endIf();
      } else if (elseBody) {
        throw new Error('CodeGen: "else" body without "then" body');
      }
      return this;
    }
    elseIf(condition) {
      return this._elseNode(new If(condition));
    }
    else() {
      return this._elseNode(new Else);
    }
    endIf() {
      return this._endBlockNode(If, Else);
    }
    _for(node, forBody) {
      this._blockNode(node);
      if (forBody)
        this.code(forBody).endFor();
      return this;
    }
    for(iteration, forBody) {
      return this._for(new ForLoop(iteration), forBody);
    }
    forRange(nameOrPrefix, from, to, forBody, varKind = this.opts.es5 ? scope_1.varKinds.var : scope_1.varKinds.let) {
      const name = this._scope.toName(nameOrPrefix);
      return this._for(new ForRange(varKind, name, from, to), () => forBody(name));
    }
    forOf(nameOrPrefix, iterable, forBody, varKind = scope_1.varKinds.const) {
      const name = this._scope.toName(nameOrPrefix);
      if (this.opts.es5) {
        const arr = iterable instanceof code_1.Name ? iterable : this.var("_arr", iterable);
        return this.forRange("_i", 0, (0, code_1._)`${arr}.length`, (i) => {
          this.var(name, (0, code_1._)`${arr}[${i}]`);
          forBody(name);
        });
      }
      return this._for(new ForIter("of", varKind, name, iterable), () => forBody(name));
    }
    forIn(nameOrPrefix, obj, forBody, varKind = this.opts.es5 ? scope_1.varKinds.var : scope_1.varKinds.const) {
      if (this.opts.ownProperties) {
        return this.forOf(nameOrPrefix, (0, code_1._)`Object.keys(${obj})`, forBody);
      }
      const name = this._scope.toName(nameOrPrefix);
      return this._for(new ForIter("in", varKind, name, obj), () => forBody(name));
    }
    endFor() {
      return this._endBlockNode(For);
    }
    label(label) {
      return this._leafNode(new Label(label));
    }
    break(label) {
      return this._leafNode(new Break(label));
    }
    return(value) {
      const node = new Return;
      this._blockNode(node);
      this.code(value);
      if (node.nodes.length !== 1)
        throw new Error('CodeGen: "return" should have one node');
      return this._endBlockNode(Return);
    }
    try(tryBody, catchCode, finallyCode) {
      if (!catchCode && !finallyCode)
        throw new Error('CodeGen: "try" without "catch" and "finally"');
      const node = new Try;
      this._blockNode(node);
      this.code(tryBody);
      if (catchCode) {
        const error = this.name("e");
        this._currNode = node.catch = new Catch(error);
        catchCode(error);
      }
      if (finallyCode) {
        this._currNode = node.finally = new Finally;
        this.code(finallyCode);
      }
      return this._endBlockNode(Catch, Finally);
    }
    throw(error) {
      return this._leafNode(new Throw(error));
    }
    block(body, nodeCount) {
      this._blockStarts.push(this._nodes.length);
      if (body)
        this.code(body).endBlock(nodeCount);
      return this;
    }
    endBlock(nodeCount) {
      const len = this._blockStarts.pop();
      if (len === undefined)
        throw new Error("CodeGen: not in self-balancing block");
      const toClose = this._nodes.length - len;
      if (toClose < 0 || nodeCount !== undefined && toClose !== nodeCount) {
        throw new Error(`CodeGen: wrong number of nodes: ${toClose} vs ${nodeCount} expected`);
      }
      this._nodes.length = len;
      return this;
    }
    func(name, args = code_1.nil, async, funcBody) {
      this._blockNode(new Func(name, args, async));
      if (funcBody)
        this.code(funcBody).endFunc();
      return this;
    }
    endFunc() {
      return this._endBlockNode(Func);
    }
    optimize(n = 1) {
      while (n-- > 0) {
        this._root.optimizeNodes();
        this._root.optimizeNames(this._root.names, this._constants);
      }
    }
    _leafNode(node) {
      this._currNode.nodes.push(node);
      return this;
    }
    _blockNode(node) {
      this._currNode.nodes.push(node);
      this._nodes.push(node);
    }
    _endBlockNode(N1, N2) {
      const n = this._currNode;
      if (n instanceof N1 || N2 && n instanceof N2) {
        this._nodes.pop();
        return this;
      }
      throw new Error(`CodeGen: not in block "${N2 ? `${N1.kind}/${N2.kind}` : N1.kind}"`);
    }
    _elseNode(node) {
      const n = this._currNode;
      if (!(n instanceof If)) {
        throw new Error('CodeGen: "else" without "if"');
      }
      this._currNode = n.else = node;
      return this;
    }
    get _root() {
      return this._nodes[0];
    }
    get _currNode() {
      const ns = this._nodes;
      return ns[ns.length - 1];
    }
    set _currNode(node) {
      const ns = this._nodes;
      ns[ns.length - 1] = node;
    }
  }
  exports.CodeGen = CodeGen;
  function addNames(names, from) {
    for (const n in from)
      names[n] = (names[n] || 0) + (from[n] || 0);
    return names;
  }
  function addExprNames(names, from) {
    return from instanceof code_1._CodeOrName ? addNames(names, from.names) : names;
  }
  function optimizeExpr(expr, names, constants) {
    if (expr instanceof code_1.Name)
      return replaceName(expr);
    if (!canOptimize(expr))
      return expr;
    return new code_1._Code(expr._items.reduce((items, c) => {
      if (c instanceof code_1.Name)
        c = replaceName(c);
      if (c instanceof code_1._Code)
        items.push(...c._items);
      else
        items.push(c);
      return items;
    }, []));
    function replaceName(n) {
      const c = constants[n.str];
      if (c === undefined || names[n.str] !== 1)
        return n;
      delete names[n.str];
      return c;
    }
    function canOptimize(e) {
      return e instanceof code_1._Code && e._items.some((c) => c instanceof code_1.Name && names[c.str] === 1 && constants[c.str] !== undefined);
    }
  }
  function subtractNames(names, from) {
    for (const n in from)
      names[n] = (names[n] || 0) - (from[n] || 0);
  }
  function not(x) {
    return typeof x == "boolean" || typeof x == "number" || x === null ? !x : (0, code_1._)`!${par(x)}`;
  }
  exports.not = not;
  var andCode = mappend(exports.operators.AND);
  function and(...args) {
    return args.reduce(andCode);
  }
  exports.and = and;
  var orCode = mappend(exports.operators.OR);
  function or(...args) {
    return args.reduce(orCode);
  }
  exports.or = or;
  function mappend(op) {
    return (x, y) => x === code_1.nil ? y : y === code_1.nil ? x : (0, code_1._)`${par(x)} ${op} ${par(y)}`;
  }
  function par(x) {
    return x instanceof code_1.Name ? x : (0, code_1._)`(${x})`;
  }
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/compile/util.js
var require_util = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.checkStrictMode = exports.getErrorPath = exports.Type = exports.useFunc = exports.setEvaluated = exports.evaluatedPropsToName = exports.mergeEvaluated = exports.eachItem = exports.unescapeJsonPointer = exports.escapeJsonPointer = exports.escapeFragment = exports.unescapeFragment = exports.schemaRefOrVal = exports.schemaHasRulesButRef = exports.schemaHasRules = exports.checkUnknownRules = exports.alwaysValidSchema = exports.toHash = undefined;
  var codegen_1 = require_codegen();
  var code_1 = require_code();
  function toHash(arr) {
    const hash = {};
    for (const item of arr)
      hash[item] = true;
    return hash;
  }
  exports.toHash = toHash;
  function alwaysValidSchema(it, schema) {
    if (typeof schema == "boolean")
      return schema;
    if (Object.keys(schema).length === 0)
      return true;
    checkUnknownRules(it, schema);
    return !schemaHasRules(schema, it.self.RULES.all);
  }
  exports.alwaysValidSchema = alwaysValidSchema;
  function checkUnknownRules(it, schema = it.schema) {
    const { opts, self } = it;
    if (!opts.strictSchema)
      return;
    if (typeof schema === "boolean")
      return;
    const rules = self.RULES.keywords;
    for (const key in schema) {
      if (!rules[key])
        checkStrictMode(it, `unknown keyword: "${key}"`);
    }
  }
  exports.checkUnknownRules = checkUnknownRules;
  function schemaHasRules(schema, rules) {
    if (typeof schema == "boolean")
      return !schema;
    for (const key in schema)
      if (rules[key])
        return true;
    return false;
  }
  exports.schemaHasRules = schemaHasRules;
  function schemaHasRulesButRef(schema, RULES) {
    if (typeof schema == "boolean")
      return !schema;
    for (const key in schema)
      if (key !== "$ref" && RULES.all[key])
        return true;
    return false;
  }
  exports.schemaHasRulesButRef = schemaHasRulesButRef;
  function schemaRefOrVal({ topSchemaRef, schemaPath }, schema, keyword, $data) {
    if (!$data) {
      if (typeof schema == "number" || typeof schema == "boolean")
        return schema;
      if (typeof schema == "string")
        return (0, codegen_1._)`${schema}`;
    }
    return (0, codegen_1._)`${topSchemaRef}${schemaPath}${(0, codegen_1.getProperty)(keyword)}`;
  }
  exports.schemaRefOrVal = schemaRefOrVal;
  function unescapeFragment(str) {
    return unescapeJsonPointer(decodeURIComponent(str));
  }
  exports.unescapeFragment = unescapeFragment;
  function escapeFragment(str) {
    return encodeURIComponent(escapeJsonPointer(str));
  }
  exports.escapeFragment = escapeFragment;
  function escapeJsonPointer(str) {
    if (typeof str == "number")
      return `${str}`;
    return str.replace(/~/g, "~0").replace(/\//g, "~1");
  }
  exports.escapeJsonPointer = escapeJsonPointer;
  function unescapeJsonPointer(str) {
    return str.replace(/~1/g, "/").replace(/~0/g, "~");
  }
  exports.unescapeJsonPointer = unescapeJsonPointer;
  function eachItem(xs, f) {
    if (Array.isArray(xs)) {
      for (const x of xs)
        f(x);
    } else {
      f(xs);
    }
  }
  exports.eachItem = eachItem;
  function makeMergeEvaluated({ mergeNames, mergeToName, mergeValues, resultToName }) {
    return (gen, from, to, toName) => {
      const res = to === undefined ? from : to instanceof codegen_1.Name ? (from instanceof codegen_1.Name ? mergeNames(gen, from, to) : mergeToName(gen, from, to), to) : from instanceof codegen_1.Name ? (mergeToName(gen, to, from), from) : mergeValues(from, to);
      return toName === codegen_1.Name && !(res instanceof codegen_1.Name) ? resultToName(gen, res) : res;
    };
  }
  exports.mergeEvaluated = {
    props: makeMergeEvaluated({
      mergeNames: (gen, from, to) => gen.if((0, codegen_1._)`${to} !== true && ${from} !== undefined`, () => {
        gen.if((0, codegen_1._)`${from} === true`, () => gen.assign(to, true), () => gen.assign(to, (0, codegen_1._)`${to} || {}`).code((0, codegen_1._)`Object.assign(${to}, ${from})`));
      }),
      mergeToName: (gen, from, to) => gen.if((0, codegen_1._)`${to} !== true`, () => {
        if (from === true) {
          gen.assign(to, true);
        } else {
          gen.assign(to, (0, codegen_1._)`${to} || {}`);
          setEvaluated(gen, to, from);
        }
      }),
      mergeValues: (from, to) => from === true ? true : { ...from, ...to },
      resultToName: evaluatedPropsToName
    }),
    items: makeMergeEvaluated({
      mergeNames: (gen, from, to) => gen.if((0, codegen_1._)`${to} !== true && ${from} !== undefined`, () => gen.assign(to, (0, codegen_1._)`${from} === true ? true : ${to} > ${from} ? ${to} : ${from}`)),
      mergeToName: (gen, from, to) => gen.if((0, codegen_1._)`${to} !== true`, () => gen.assign(to, from === true ? true : (0, codegen_1._)`${to} > ${from} ? ${to} : ${from}`)),
      mergeValues: (from, to) => from === true ? true : Math.max(from, to),
      resultToName: (gen, items) => gen.var("items", items)
    })
  };
  function evaluatedPropsToName(gen, ps) {
    if (ps === true)
      return gen.var("props", true);
    const props = gen.var("props", (0, codegen_1._)`{}`);
    if (ps !== undefined)
      setEvaluated(gen, props, ps);
    return props;
  }
  exports.evaluatedPropsToName = evaluatedPropsToName;
  function setEvaluated(gen, props, ps) {
    Object.keys(ps).forEach((p) => gen.assign((0, codegen_1._)`${props}${(0, codegen_1.getProperty)(p)}`, true));
  }
  exports.setEvaluated = setEvaluated;
  var snippets = {};
  function useFunc(gen, f) {
    return gen.scopeValue("func", {
      ref: f,
      code: snippets[f.code] || (snippets[f.code] = new code_1._Code(f.code))
    });
  }
  exports.useFunc = useFunc;
  var Type;
  (function(Type2) {
    Type2[Type2["Num"] = 0] = "Num";
    Type2[Type2["Str"] = 1] = "Str";
  })(Type || (exports.Type = Type = {}));
  function getErrorPath(dataProp, dataPropType, jsPropertySyntax) {
    if (dataProp instanceof codegen_1.Name) {
      const isNumber = dataPropType === Type.Num;
      return jsPropertySyntax ? isNumber ? (0, codegen_1._)`"[" + ${dataProp} + "]"` : (0, codegen_1._)`"['" + ${dataProp} + "']"` : isNumber ? (0, codegen_1._)`"/" + ${dataProp}` : (0, codegen_1._)`"/" + ${dataProp}.replace(/~/g, "~0").replace(/\\//g, "~1")`;
    }
    return jsPropertySyntax ? (0, codegen_1.getProperty)(dataProp).toString() : "/" + escapeJsonPointer(dataProp);
  }
  exports.getErrorPath = getErrorPath;
  function checkStrictMode(it, msg, mode = it.opts.strictSchema) {
    if (!mode)
      return;
    msg = `strict mode: ${msg}`;
    if (mode === true)
      throw new Error(msg);
    it.self.logger.warn(msg);
  }
  exports.checkStrictMode = checkStrictMode;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/compile/names.js
var require_names = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var codegen_1 = require_codegen();
  var names = {
    data: new codegen_1.Name("data"),
    valCxt: new codegen_1.Name("valCxt"),
    instancePath: new codegen_1.Name("instancePath"),
    parentData: new codegen_1.Name("parentData"),
    parentDataProperty: new codegen_1.Name("parentDataProperty"),
    rootData: new codegen_1.Name("rootData"),
    dynamicAnchors: new codegen_1.Name("dynamicAnchors"),
    vErrors: new codegen_1.Name("vErrors"),
    errors: new codegen_1.Name("errors"),
    this: new codegen_1.Name("this"),
    self: new codegen_1.Name("self"),
    scope: new codegen_1.Name("scope"),
    json: new codegen_1.Name("json"),
    jsonPos: new codegen_1.Name("jsonPos"),
    jsonLen: new codegen_1.Name("jsonLen"),
    jsonPart: new codegen_1.Name("jsonPart")
  };
  exports.default = names;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/compile/errors.js
var require_errors = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.extendErrors = exports.resetErrorsCount = exports.reportExtraError = exports.reportError = exports.keyword$DataError = exports.keywordError = undefined;
  var codegen_1 = require_codegen();
  var util_1 = require_util();
  var names_1 = require_names();
  exports.keywordError = {
    message: ({ keyword }) => (0, codegen_1.str)`must pass "${keyword}" keyword validation`
  };
  exports.keyword$DataError = {
    message: ({ keyword, schemaType }) => schemaType ? (0, codegen_1.str)`"${keyword}" keyword must be ${schemaType} ($data)` : (0, codegen_1.str)`"${keyword}" keyword is invalid ($data)`
  };
  function reportError(cxt, error = exports.keywordError, errorPaths, overrideAllErrors) {
    const { it } = cxt;
    const { gen, compositeRule, allErrors } = it;
    const errObj = errorObjectCode(cxt, error, errorPaths);
    if (overrideAllErrors !== null && overrideAllErrors !== undefined ? overrideAllErrors : compositeRule || allErrors) {
      addError(gen, errObj);
    } else {
      returnErrors(it, (0, codegen_1._)`[${errObj}]`);
    }
  }
  exports.reportError = reportError;
  function reportExtraError(cxt, error = exports.keywordError, errorPaths) {
    const { it } = cxt;
    const { gen, compositeRule, allErrors } = it;
    const errObj = errorObjectCode(cxt, error, errorPaths);
    addError(gen, errObj);
    if (!(compositeRule || allErrors)) {
      returnErrors(it, names_1.default.vErrors);
    }
  }
  exports.reportExtraError = reportExtraError;
  function resetErrorsCount(gen, errsCount) {
    gen.assign(names_1.default.errors, errsCount);
    gen.if((0, codegen_1._)`${names_1.default.vErrors} !== null`, () => gen.if(errsCount, () => gen.assign((0, codegen_1._)`${names_1.default.vErrors}.length`, errsCount), () => gen.assign(names_1.default.vErrors, null)));
  }
  exports.resetErrorsCount = resetErrorsCount;
  function extendErrors({ gen, keyword, schemaValue, data, errsCount, it }) {
    if (errsCount === undefined)
      throw new Error("ajv implementation error");
    const err = gen.name("err");
    gen.forRange("i", errsCount, names_1.default.errors, (i) => {
      gen.const(err, (0, codegen_1._)`${names_1.default.vErrors}[${i}]`);
      gen.if((0, codegen_1._)`${err}.instancePath === undefined`, () => gen.assign((0, codegen_1._)`${err}.instancePath`, (0, codegen_1.strConcat)(names_1.default.instancePath, it.errorPath)));
      gen.assign((0, codegen_1._)`${err}.schemaPath`, (0, codegen_1.str)`${it.errSchemaPath}/${keyword}`);
      if (it.opts.verbose) {
        gen.assign((0, codegen_1._)`${err}.schema`, schemaValue);
        gen.assign((0, codegen_1._)`${err}.data`, data);
      }
    });
  }
  exports.extendErrors = extendErrors;
  function addError(gen, errObj) {
    const err = gen.const("err", errObj);
    gen.if((0, codegen_1._)`${names_1.default.vErrors} === null`, () => gen.assign(names_1.default.vErrors, (0, codegen_1._)`[${err}]`), (0, codegen_1._)`${names_1.default.vErrors}.push(${err})`);
    gen.code((0, codegen_1._)`${names_1.default.errors}++`);
  }
  function returnErrors(it, errs) {
    const { gen, validateName, schemaEnv } = it;
    if (schemaEnv.$async) {
      gen.throw((0, codegen_1._)`new ${it.ValidationError}(${errs})`);
    } else {
      gen.assign((0, codegen_1._)`${validateName}.errors`, errs);
      gen.return(false);
    }
  }
  var E = {
    keyword: new codegen_1.Name("keyword"),
    schemaPath: new codegen_1.Name("schemaPath"),
    params: new codegen_1.Name("params"),
    propertyName: new codegen_1.Name("propertyName"),
    message: new codegen_1.Name("message"),
    schema: new codegen_1.Name("schema"),
    parentSchema: new codegen_1.Name("parentSchema")
  };
  function errorObjectCode(cxt, error, errorPaths) {
    const { createErrors } = cxt.it;
    if (createErrors === false)
      return (0, codegen_1._)`{}`;
    return errorObject(cxt, error, errorPaths);
  }
  function errorObject(cxt, error, errorPaths = {}) {
    const { gen, it } = cxt;
    const keyValues = [
      errorInstancePath(it, errorPaths),
      errorSchemaPath(cxt, errorPaths)
    ];
    extraErrorProps(cxt, error, keyValues);
    return gen.object(...keyValues);
  }
  function errorInstancePath({ errorPath }, { instancePath }) {
    const instPath = instancePath ? (0, codegen_1.str)`${errorPath}${(0, util_1.getErrorPath)(instancePath, util_1.Type.Str)}` : errorPath;
    return [names_1.default.instancePath, (0, codegen_1.strConcat)(names_1.default.instancePath, instPath)];
  }
  function errorSchemaPath({ keyword, it: { errSchemaPath } }, { schemaPath, parentSchema }) {
    let schPath = parentSchema ? errSchemaPath : (0, codegen_1.str)`${errSchemaPath}/${keyword}`;
    if (schemaPath) {
      schPath = (0, codegen_1.str)`${schPath}${(0, util_1.getErrorPath)(schemaPath, util_1.Type.Str)}`;
    }
    return [E.schemaPath, schPath];
  }
  function extraErrorProps(cxt, { params, message }, keyValues) {
    const { keyword, data, schemaValue, it } = cxt;
    const { opts, propertyName, topSchemaRef, schemaPath } = it;
    keyValues.push([E.keyword, keyword], [E.params, typeof params == "function" ? params(cxt) : params || (0, codegen_1._)`{}`]);
    if (opts.messages) {
      keyValues.push([E.message, typeof message == "function" ? message(cxt) : message]);
    }
    if (opts.verbose) {
      keyValues.push([E.schema, schemaValue], [E.parentSchema, (0, codegen_1._)`${topSchemaRef}${schemaPath}`], [names_1.default.data, data]);
    }
    if (propertyName)
      keyValues.push([E.propertyName, propertyName]);
  }
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/compile/validate/boolSchema.js
var require_boolSchema = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.boolOrEmptySchema = exports.topBoolOrEmptySchema = undefined;
  var errors_1 = require_errors();
  var codegen_1 = require_codegen();
  var names_1 = require_names();
  var boolError = {
    message: "boolean schema is false"
  };
  function topBoolOrEmptySchema(it) {
    const { gen, schema, validateName } = it;
    if (schema === false) {
      falseSchemaError(it, false);
    } else if (typeof schema == "object" && schema.$async === true) {
      gen.return(names_1.default.data);
    } else {
      gen.assign((0, codegen_1._)`${validateName}.errors`, null);
      gen.return(true);
    }
  }
  exports.topBoolOrEmptySchema = topBoolOrEmptySchema;
  function boolOrEmptySchema(it, valid) {
    const { gen, schema } = it;
    if (schema === false) {
      gen.var(valid, false);
      falseSchemaError(it);
    } else {
      gen.var(valid, true);
    }
  }
  exports.boolOrEmptySchema = boolOrEmptySchema;
  function falseSchemaError(it, overrideAllErrors) {
    const { gen, data } = it;
    const cxt = {
      gen,
      keyword: "false schema",
      data,
      schema: false,
      schemaCode: false,
      schemaValue: false,
      params: {},
      it
    };
    (0, errors_1.reportError)(cxt, boolError, undefined, overrideAllErrors);
  }
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/compile/rules.js
var require_rules = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.getRules = exports.isJSONType = undefined;
  var _jsonTypes = ["string", "number", "integer", "boolean", "null", "object", "array"];
  var jsonTypes = new Set(_jsonTypes);
  function isJSONType(x) {
    return typeof x == "string" && jsonTypes.has(x);
  }
  exports.isJSONType = isJSONType;
  function getRules() {
    const groups = {
      number: { type: "number", rules: [] },
      string: { type: "string", rules: [] },
      array: { type: "array", rules: [] },
      object: { type: "object", rules: [] }
    };
    return {
      types: { ...groups, integer: true, boolean: true, null: true },
      rules: [{ rules: [] }, groups.number, groups.string, groups.array, groups.object],
      post: { rules: [] },
      all: {},
      keywords: {}
    };
  }
  exports.getRules = getRules;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/compile/validate/applicability.js
var require_applicability = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.shouldUseRule = exports.shouldUseGroup = exports.schemaHasRulesForType = undefined;
  function schemaHasRulesForType({ schema, self }, type) {
    const group = self.RULES.types[type];
    return group && group !== true && shouldUseGroup(schema, group);
  }
  exports.schemaHasRulesForType = schemaHasRulesForType;
  function shouldUseGroup(schema, group) {
    return group.rules.some((rule) => shouldUseRule(schema, rule));
  }
  exports.shouldUseGroup = shouldUseGroup;
  function shouldUseRule(schema, rule) {
    var _a;
    return schema[rule.keyword] !== undefined || ((_a = rule.definition.implements) === null || _a === undefined ? undefined : _a.some((kwd) => schema[kwd] !== undefined));
  }
  exports.shouldUseRule = shouldUseRule;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/compile/validate/dataType.js
var require_dataType = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.reportTypeError = exports.checkDataTypes = exports.checkDataType = exports.coerceAndCheckDataType = exports.getJSONTypes = exports.getSchemaTypes = exports.DataType = undefined;
  var rules_1 = require_rules();
  var applicability_1 = require_applicability();
  var errors_1 = require_errors();
  var codegen_1 = require_codegen();
  var util_1 = require_util();
  var DataType;
  (function(DataType2) {
    DataType2[DataType2["Correct"] = 0] = "Correct";
    DataType2[DataType2["Wrong"] = 1] = "Wrong";
  })(DataType || (exports.DataType = DataType = {}));
  function getSchemaTypes(schema) {
    const types = getJSONTypes(schema.type);
    const hasNull = types.includes("null");
    if (hasNull) {
      if (schema.nullable === false)
        throw new Error("type: null contradicts nullable: false");
    } else {
      if (!types.length && schema.nullable !== undefined) {
        throw new Error('"nullable" cannot be used without "type"');
      }
      if (schema.nullable === true)
        types.push("null");
    }
    return types;
  }
  exports.getSchemaTypes = getSchemaTypes;
  function getJSONTypes(ts) {
    const types = Array.isArray(ts) ? ts : ts ? [ts] : [];
    if (types.every(rules_1.isJSONType))
      return types;
    throw new Error("type must be JSONType or JSONType[]: " + types.join(","));
  }
  exports.getJSONTypes = getJSONTypes;
  function coerceAndCheckDataType(it, types) {
    const { gen, data, opts } = it;
    const coerceTo = coerceToTypes(types, opts.coerceTypes);
    const checkTypes = types.length > 0 && !(coerceTo.length === 0 && types.length === 1 && (0, applicability_1.schemaHasRulesForType)(it, types[0]));
    if (checkTypes) {
      const wrongType = checkDataTypes(types, data, opts.strictNumbers, DataType.Wrong);
      gen.if(wrongType, () => {
        if (coerceTo.length)
          coerceData(it, types, coerceTo);
        else
          reportTypeError(it);
      });
    }
    return checkTypes;
  }
  exports.coerceAndCheckDataType = coerceAndCheckDataType;
  var COERCIBLE = new Set(["string", "number", "integer", "boolean", "null"]);
  function coerceToTypes(types, coerceTypes) {
    return coerceTypes ? types.filter((t) => COERCIBLE.has(t) || coerceTypes === "array" && t === "array") : [];
  }
  function coerceData(it, types, coerceTo) {
    const { gen, data, opts } = it;
    const dataType = gen.let("dataType", (0, codegen_1._)`typeof ${data}`);
    const coerced = gen.let("coerced", (0, codegen_1._)`undefined`);
    if (opts.coerceTypes === "array") {
      gen.if((0, codegen_1._)`${dataType} == 'object' && Array.isArray(${data}) && ${data}.length == 1`, () => gen.assign(data, (0, codegen_1._)`${data}[0]`).assign(dataType, (0, codegen_1._)`typeof ${data}`).if(checkDataTypes(types, data, opts.strictNumbers), () => gen.assign(coerced, data)));
    }
    gen.if((0, codegen_1._)`${coerced} !== undefined`);
    for (const t of coerceTo) {
      if (COERCIBLE.has(t) || t === "array" && opts.coerceTypes === "array") {
        coerceSpecificType(t);
      }
    }
    gen.else();
    reportTypeError(it);
    gen.endIf();
    gen.if((0, codegen_1._)`${coerced} !== undefined`, () => {
      gen.assign(data, coerced);
      assignParentData(it, coerced);
    });
    function coerceSpecificType(t) {
      switch (t) {
        case "string":
          gen.elseIf((0, codegen_1._)`${dataType} == "number" || ${dataType} == "boolean"`).assign(coerced, (0, codegen_1._)`"" + ${data}`).elseIf((0, codegen_1._)`${data} === null`).assign(coerced, (0, codegen_1._)`""`);
          return;
        case "number":
          gen.elseIf((0, codegen_1._)`${dataType} == "boolean" || ${data} === null
              || (${dataType} == "string" && ${data} && ${data} == +${data})`).assign(coerced, (0, codegen_1._)`+${data}`);
          return;
        case "integer":
          gen.elseIf((0, codegen_1._)`${dataType} === "boolean" || ${data} === null
              || (${dataType} === "string" && ${data} && ${data} == +${data} && !(${data} % 1))`).assign(coerced, (0, codegen_1._)`+${data}`);
          return;
        case "boolean":
          gen.elseIf((0, codegen_1._)`${data} === "false" || ${data} === 0 || ${data} === null`).assign(coerced, false).elseIf((0, codegen_1._)`${data} === "true" || ${data} === 1`).assign(coerced, true);
          return;
        case "null":
          gen.elseIf((0, codegen_1._)`${data} === "" || ${data} === 0 || ${data} === false`);
          gen.assign(coerced, null);
          return;
        case "array":
          gen.elseIf((0, codegen_1._)`${dataType} === "string" || ${dataType} === "number"
              || ${dataType} === "boolean" || ${data} === null`).assign(coerced, (0, codegen_1._)`[${data}]`);
      }
    }
  }
  function assignParentData({ gen, parentData, parentDataProperty }, expr) {
    gen.if((0, codegen_1._)`${parentData} !== undefined`, () => gen.assign((0, codegen_1._)`${parentData}[${parentDataProperty}]`, expr));
  }
  function checkDataType(dataType, data, strictNums, correct = DataType.Correct) {
    const EQ = correct === DataType.Correct ? codegen_1.operators.EQ : codegen_1.operators.NEQ;
    let cond;
    switch (dataType) {
      case "null":
        return (0, codegen_1._)`${data} ${EQ} null`;
      case "array":
        cond = (0, codegen_1._)`Array.isArray(${data})`;
        break;
      case "object":
        cond = (0, codegen_1._)`${data} && typeof ${data} == "object" && !Array.isArray(${data})`;
        break;
      case "integer":
        cond = numCond((0, codegen_1._)`!(${data} % 1) && !isNaN(${data})`);
        break;
      case "number":
        cond = numCond();
        break;
      default:
        return (0, codegen_1._)`typeof ${data} ${EQ} ${dataType}`;
    }
    return correct === DataType.Correct ? cond : (0, codegen_1.not)(cond);
    function numCond(_cond = codegen_1.nil) {
      return (0, codegen_1.and)((0, codegen_1._)`typeof ${data} == "number"`, _cond, strictNums ? (0, codegen_1._)`isFinite(${data})` : codegen_1.nil);
    }
  }
  exports.checkDataType = checkDataType;
  function checkDataTypes(dataTypes, data, strictNums, correct) {
    if (dataTypes.length === 1) {
      return checkDataType(dataTypes[0], data, strictNums, correct);
    }
    let cond;
    const types = (0, util_1.toHash)(dataTypes);
    if (types.array && types.object) {
      const notObj = (0, codegen_1._)`typeof ${data} != "object"`;
      cond = types.null ? notObj : (0, codegen_1._)`!${data} || ${notObj}`;
      delete types.null;
      delete types.array;
      delete types.object;
    } else {
      cond = codegen_1.nil;
    }
    if (types.number)
      delete types.integer;
    for (const t in types)
      cond = (0, codegen_1.and)(cond, checkDataType(t, data, strictNums, correct));
    return cond;
  }
  exports.checkDataTypes = checkDataTypes;
  var typeError = {
    message: ({ schema }) => `must be ${schema}`,
    params: ({ schema, schemaValue }) => typeof schema == "string" ? (0, codegen_1._)`{type: ${schema}}` : (0, codegen_1._)`{type: ${schemaValue}}`
  };
  function reportTypeError(it) {
    const cxt = getTypeErrorContext(it);
    (0, errors_1.reportError)(cxt, typeError);
  }
  exports.reportTypeError = reportTypeError;
  function getTypeErrorContext(it) {
    const { gen, data, schema } = it;
    const schemaCode = (0, util_1.schemaRefOrVal)(it, schema, "type");
    return {
      gen,
      keyword: "type",
      data,
      schema: schema.type,
      schemaCode,
      schemaValue: schemaCode,
      parentSchema: schema,
      params: {},
      it
    };
  }
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/compile/validate/defaults.js
var require_defaults = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.assignDefaults = undefined;
  var codegen_1 = require_codegen();
  var util_1 = require_util();
  function assignDefaults(it, ty) {
    const { properties, items } = it.schema;
    if (ty === "object" && properties) {
      for (const key in properties) {
        assignDefault(it, key, properties[key].default);
      }
    } else if (ty === "array" && Array.isArray(items)) {
      items.forEach((sch, i) => assignDefault(it, i, sch.default));
    }
  }
  exports.assignDefaults = assignDefaults;
  function assignDefault(it, prop, defaultValue) {
    const { gen, compositeRule, data, opts } = it;
    if (defaultValue === undefined)
      return;
    const childData = (0, codegen_1._)`${data}${(0, codegen_1.getProperty)(prop)}`;
    if (compositeRule) {
      (0, util_1.checkStrictMode)(it, `default is ignored for: ${childData}`);
      return;
    }
    let condition = (0, codegen_1._)`${childData} === undefined`;
    if (opts.useDefaults === "empty") {
      condition = (0, codegen_1._)`${condition} || ${childData} === null || ${childData} === ""`;
    }
    gen.if(condition, (0, codegen_1._)`${childData} = ${(0, codegen_1.stringify)(defaultValue)}`);
  }
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/code.js
var require_code2 = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.validateUnion = exports.validateArray = exports.usePattern = exports.callValidateCode = exports.schemaProperties = exports.allSchemaProperties = exports.noPropertyInData = exports.propertyInData = exports.isOwnProperty = exports.hasPropFunc = exports.reportMissingProp = exports.checkMissingProp = exports.checkReportMissingProp = undefined;
  var codegen_1 = require_codegen();
  var util_1 = require_util();
  var names_1 = require_names();
  var util_2 = require_util();
  function checkReportMissingProp(cxt, prop) {
    const { gen, data, it } = cxt;
    gen.if(noPropertyInData(gen, data, prop, it.opts.ownProperties), () => {
      cxt.setParams({ missingProperty: (0, codegen_1._)`${prop}` }, true);
      cxt.error();
    });
  }
  exports.checkReportMissingProp = checkReportMissingProp;
  function checkMissingProp({ gen, data, it: { opts } }, properties, missing) {
    return (0, codegen_1.or)(...properties.map((prop) => (0, codegen_1.and)(noPropertyInData(gen, data, prop, opts.ownProperties), (0, codegen_1._)`${missing} = ${prop}`)));
  }
  exports.checkMissingProp = checkMissingProp;
  function reportMissingProp(cxt, missing) {
    cxt.setParams({ missingProperty: missing }, true);
    cxt.error();
  }
  exports.reportMissingProp = reportMissingProp;
  function hasPropFunc(gen) {
    return gen.scopeValue("func", {
      ref: Object.prototype.hasOwnProperty,
      code: (0, codegen_1._)`Object.prototype.hasOwnProperty`
    });
  }
  exports.hasPropFunc = hasPropFunc;
  function isOwnProperty(gen, data, property) {
    return (0, codegen_1._)`${hasPropFunc(gen)}.call(${data}, ${property})`;
  }
  exports.isOwnProperty = isOwnProperty;
  function propertyInData(gen, data, property, ownProperties) {
    const cond = (0, codegen_1._)`${data}${(0, codegen_1.getProperty)(property)} !== undefined`;
    return ownProperties ? (0, codegen_1._)`${cond} && ${isOwnProperty(gen, data, property)}` : cond;
  }
  exports.propertyInData = propertyInData;
  function noPropertyInData(gen, data, property, ownProperties) {
    const cond = (0, codegen_1._)`${data}${(0, codegen_1.getProperty)(property)} === undefined`;
    return ownProperties ? (0, codegen_1.or)(cond, (0, codegen_1.not)(isOwnProperty(gen, data, property))) : cond;
  }
  exports.noPropertyInData = noPropertyInData;
  function allSchemaProperties(schemaMap) {
    return schemaMap ? Object.keys(schemaMap).filter((p) => p !== "__proto__") : [];
  }
  exports.allSchemaProperties = allSchemaProperties;
  function schemaProperties(it, schemaMap) {
    return allSchemaProperties(schemaMap).filter((p) => !(0, util_1.alwaysValidSchema)(it, schemaMap[p]));
  }
  exports.schemaProperties = schemaProperties;
  function callValidateCode({ schemaCode, data, it: { gen, topSchemaRef, schemaPath, errorPath }, it }, func, context, passSchema) {
    const dataAndSchema = passSchema ? (0, codegen_1._)`${schemaCode}, ${data}, ${topSchemaRef}${schemaPath}` : data;
    const valCxt = [
      [names_1.default.instancePath, (0, codegen_1.strConcat)(names_1.default.instancePath, errorPath)],
      [names_1.default.parentData, it.parentData],
      [names_1.default.parentDataProperty, it.parentDataProperty],
      [names_1.default.rootData, names_1.default.rootData]
    ];
    if (it.opts.dynamicRef)
      valCxt.push([names_1.default.dynamicAnchors, names_1.default.dynamicAnchors]);
    const args = (0, codegen_1._)`${dataAndSchema}, ${gen.object(...valCxt)}`;
    return context !== codegen_1.nil ? (0, codegen_1._)`${func}.call(${context}, ${args})` : (0, codegen_1._)`${func}(${args})`;
  }
  exports.callValidateCode = callValidateCode;
  var newRegExp = (0, codegen_1._)`new RegExp`;
  function usePattern({ gen, it: { opts } }, pattern) {
    const u = opts.unicodeRegExp ? "u" : "";
    const { regExp } = opts.code;
    const rx = regExp(pattern, u);
    return gen.scopeValue("pattern", {
      key: rx.toString(),
      ref: rx,
      code: (0, codegen_1._)`${regExp.code === "new RegExp" ? newRegExp : (0, util_2.useFunc)(gen, regExp)}(${pattern}, ${u})`
    });
  }
  exports.usePattern = usePattern;
  function validateArray(cxt) {
    const { gen, data, keyword, it } = cxt;
    const valid = gen.name("valid");
    if (it.allErrors) {
      const validArr = gen.let("valid", true);
      validateItems(() => gen.assign(validArr, false));
      return validArr;
    }
    gen.var(valid, true);
    validateItems(() => gen.break());
    return valid;
    function validateItems(notValid) {
      const len = gen.const("len", (0, codegen_1._)`${data}.length`);
      gen.forRange("i", 0, len, (i) => {
        cxt.subschema({
          keyword,
          dataProp: i,
          dataPropType: util_1.Type.Num
        }, valid);
        gen.if((0, codegen_1.not)(valid), notValid);
      });
    }
  }
  exports.validateArray = validateArray;
  function validateUnion(cxt) {
    const { gen, schema, keyword, it } = cxt;
    if (!Array.isArray(schema))
      throw new Error("ajv implementation error");
    const alwaysValid = schema.some((sch) => (0, util_1.alwaysValidSchema)(it, sch));
    if (alwaysValid && !it.opts.unevaluated)
      return;
    const valid = gen.let("valid", false);
    const schValid = gen.name("_valid");
    gen.block(() => schema.forEach((_sch, i) => {
      const schCxt = cxt.subschema({
        keyword,
        schemaProp: i,
        compositeRule: true
      }, schValid);
      gen.assign(valid, (0, codegen_1._)`${valid} || ${schValid}`);
      const merged = cxt.mergeValidEvaluated(schCxt, schValid);
      if (!merged)
        gen.if((0, codegen_1.not)(valid));
    }));
    cxt.result(valid, () => cxt.reset(), () => cxt.error(true));
  }
  exports.validateUnion = validateUnion;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/compile/validate/keyword.js
var require_keyword = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.validateKeywordUsage = exports.validSchemaType = exports.funcKeywordCode = exports.macroKeywordCode = undefined;
  var codegen_1 = require_codegen();
  var names_1 = require_names();
  var code_1 = require_code2();
  var errors_1 = require_errors();
  function macroKeywordCode(cxt, def) {
    const { gen, keyword, schema, parentSchema, it } = cxt;
    const macroSchema = def.macro.call(it.self, schema, parentSchema, it);
    const schemaRef = useKeyword(gen, keyword, macroSchema);
    if (it.opts.validateSchema !== false)
      it.self.validateSchema(macroSchema, true);
    const valid = gen.name("valid");
    cxt.subschema({
      schema: macroSchema,
      schemaPath: codegen_1.nil,
      errSchemaPath: `${it.errSchemaPath}/${keyword}`,
      topSchemaRef: schemaRef,
      compositeRule: true
    }, valid);
    cxt.pass(valid, () => cxt.error(true));
  }
  exports.macroKeywordCode = macroKeywordCode;
  function funcKeywordCode(cxt, def) {
    var _a;
    const { gen, keyword, schema, parentSchema, $data, it } = cxt;
    checkAsyncKeyword(it, def);
    const validate = !$data && def.compile ? def.compile.call(it.self, schema, parentSchema, it) : def.validate;
    const validateRef = useKeyword(gen, keyword, validate);
    const valid = gen.let("valid");
    cxt.block$data(valid, validateKeyword);
    cxt.ok((_a = def.valid) !== null && _a !== undefined ? _a : valid);
    function validateKeyword() {
      if (def.errors === false) {
        assignValid();
        if (def.modifying)
          modifyData(cxt);
        reportErrs(() => cxt.error());
      } else {
        const ruleErrs = def.async ? validateAsync() : validateSync();
        if (def.modifying)
          modifyData(cxt);
        reportErrs(() => addErrs(cxt, ruleErrs));
      }
    }
    function validateAsync() {
      const ruleErrs = gen.let("ruleErrs", null);
      gen.try(() => assignValid((0, codegen_1._)`await `), (e) => gen.assign(valid, false).if((0, codegen_1._)`${e} instanceof ${it.ValidationError}`, () => gen.assign(ruleErrs, (0, codegen_1._)`${e}.errors`), () => gen.throw(e)));
      return ruleErrs;
    }
    function validateSync() {
      const validateErrs = (0, codegen_1._)`${validateRef}.errors`;
      gen.assign(validateErrs, null);
      assignValid(codegen_1.nil);
      return validateErrs;
    }
    function assignValid(_await = def.async ? (0, codegen_1._)`await ` : codegen_1.nil) {
      const passCxt = it.opts.passContext ? names_1.default.this : names_1.default.self;
      const passSchema = !(("compile" in def) && !$data || def.schema === false);
      gen.assign(valid, (0, codegen_1._)`${_await}${(0, code_1.callValidateCode)(cxt, validateRef, passCxt, passSchema)}`, def.modifying);
    }
    function reportErrs(errors) {
      var _a2;
      gen.if((0, codegen_1.not)((_a2 = def.valid) !== null && _a2 !== undefined ? _a2 : valid), errors);
    }
  }
  exports.funcKeywordCode = funcKeywordCode;
  function modifyData(cxt) {
    const { gen, data, it } = cxt;
    gen.if(it.parentData, () => gen.assign(data, (0, codegen_1._)`${it.parentData}[${it.parentDataProperty}]`));
  }
  function addErrs(cxt, errs) {
    const { gen } = cxt;
    gen.if((0, codegen_1._)`Array.isArray(${errs})`, () => {
      gen.assign(names_1.default.vErrors, (0, codegen_1._)`${names_1.default.vErrors} === null ? ${errs} : ${names_1.default.vErrors}.concat(${errs})`).assign(names_1.default.errors, (0, codegen_1._)`${names_1.default.vErrors}.length`);
      (0, errors_1.extendErrors)(cxt);
    }, () => cxt.error());
  }
  function checkAsyncKeyword({ schemaEnv }, def) {
    if (def.async && !schemaEnv.$async)
      throw new Error("async keyword in sync schema");
  }
  function useKeyword(gen, keyword, result) {
    if (result === undefined)
      throw new Error(`keyword "${keyword}" failed to compile`);
    return gen.scopeValue("keyword", typeof result == "function" ? { ref: result } : { ref: result, code: (0, codegen_1.stringify)(result) });
  }
  function validSchemaType(schema, schemaType, allowUndefined = false) {
    return !schemaType.length || schemaType.some((st) => st === "array" ? Array.isArray(schema) : st === "object" ? schema && typeof schema == "object" && !Array.isArray(schema) : typeof schema == st || allowUndefined && typeof schema == "undefined");
  }
  exports.validSchemaType = validSchemaType;
  function validateKeywordUsage({ schema, opts, self, errSchemaPath }, def, keyword) {
    if (Array.isArray(def.keyword) ? !def.keyword.includes(keyword) : def.keyword !== keyword) {
      throw new Error("ajv implementation error");
    }
    const deps = def.dependencies;
    if (deps === null || deps === undefined ? undefined : deps.some((kwd) => !Object.prototype.hasOwnProperty.call(schema, kwd))) {
      throw new Error(`parent schema must have dependencies of ${keyword}: ${deps.join(",")}`);
    }
    if (def.validateSchema) {
      const valid = def.validateSchema(schema[keyword]);
      if (!valid) {
        const msg = `keyword "${keyword}" value is invalid at path "${errSchemaPath}": ` + self.errorsText(def.validateSchema.errors);
        if (opts.validateSchema === "log")
          self.logger.error(msg);
        else
          throw new Error(msg);
      }
    }
  }
  exports.validateKeywordUsage = validateKeywordUsage;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/compile/validate/subschema.js
var require_subschema = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.extendSubschemaMode = exports.extendSubschemaData = exports.getSubschema = undefined;
  var codegen_1 = require_codegen();
  var util_1 = require_util();
  function getSubschema(it, { keyword, schemaProp, schema, schemaPath, errSchemaPath, topSchemaRef }) {
    if (keyword !== undefined && schema !== undefined) {
      throw new Error('both "keyword" and "schema" passed, only one allowed');
    }
    if (keyword !== undefined) {
      const sch = it.schema[keyword];
      return schemaProp === undefined ? {
        schema: sch,
        schemaPath: (0, codegen_1._)`${it.schemaPath}${(0, codegen_1.getProperty)(keyword)}`,
        errSchemaPath: `${it.errSchemaPath}/${keyword}`
      } : {
        schema: sch[schemaProp],
        schemaPath: (0, codegen_1._)`${it.schemaPath}${(0, codegen_1.getProperty)(keyword)}${(0, codegen_1.getProperty)(schemaProp)}`,
        errSchemaPath: `${it.errSchemaPath}/${keyword}/${(0, util_1.escapeFragment)(schemaProp)}`
      };
    }
    if (schema !== undefined) {
      if (schemaPath === undefined || errSchemaPath === undefined || topSchemaRef === undefined) {
        throw new Error('"schemaPath", "errSchemaPath" and "topSchemaRef" are required with "schema"');
      }
      return {
        schema,
        schemaPath,
        topSchemaRef,
        errSchemaPath
      };
    }
    throw new Error('either "keyword" or "schema" must be passed');
  }
  exports.getSubschema = getSubschema;
  function extendSubschemaData(subschema, it, { dataProp, dataPropType: dpType, data, dataTypes, propertyName }) {
    if (data !== undefined && dataProp !== undefined) {
      throw new Error('both "data" and "dataProp" passed, only one allowed');
    }
    const { gen } = it;
    if (dataProp !== undefined) {
      const { errorPath, dataPathArr, opts } = it;
      const nextData = gen.let("data", (0, codegen_1._)`${it.data}${(0, codegen_1.getProperty)(dataProp)}`, true);
      dataContextProps(nextData);
      subschema.errorPath = (0, codegen_1.str)`${errorPath}${(0, util_1.getErrorPath)(dataProp, dpType, opts.jsPropertySyntax)}`;
      subschema.parentDataProperty = (0, codegen_1._)`${dataProp}`;
      subschema.dataPathArr = [...dataPathArr, subschema.parentDataProperty];
    }
    if (data !== undefined) {
      const nextData = data instanceof codegen_1.Name ? data : gen.let("data", data, true);
      dataContextProps(nextData);
      if (propertyName !== undefined)
        subschema.propertyName = propertyName;
    }
    if (dataTypes)
      subschema.dataTypes = dataTypes;
    function dataContextProps(_nextData) {
      subschema.data = _nextData;
      subschema.dataLevel = it.dataLevel + 1;
      subschema.dataTypes = [];
      it.definedProperties = new Set;
      subschema.parentData = it.data;
      subschema.dataNames = [...it.dataNames, _nextData];
    }
  }
  exports.extendSubschemaData = extendSubschemaData;
  function extendSubschemaMode(subschema, { jtdDiscriminator, jtdMetadata, compositeRule, createErrors, allErrors }) {
    if (compositeRule !== undefined)
      subschema.compositeRule = compositeRule;
    if (createErrors !== undefined)
      subschema.createErrors = createErrors;
    if (allErrors !== undefined)
      subschema.allErrors = allErrors;
    subschema.jtdDiscriminator = jtdDiscriminator;
    subschema.jtdMetadata = jtdMetadata;
  }
  exports.extendSubschemaMode = extendSubschemaMode;
});

// ../../node_modules/.bun/fast-deep-equal@3.1.3/node_modules/fast-deep-equal/index.js
var require_fast_deep_equal = __commonJS(function(exports, module) {
  module.exports = function equal(a, b) {
    if (a === b)
      return true;
    if (a && b && typeof a == "object" && typeof b == "object") {
      if (a.constructor !== b.constructor)
        return false;
      var length, i, keys;
      if (Array.isArray(a)) {
        length = a.length;
        if (length != b.length)
          return false;
        for (i = length;i-- !== 0; )
          if (!equal(a[i], b[i]))
            return false;
        return true;
      }
      if (a.constructor === RegExp)
        return a.source === b.source && a.flags === b.flags;
      if (a.valueOf !== Object.prototype.valueOf)
        return a.valueOf() === b.valueOf();
      if (a.toString !== Object.prototype.toString)
        return a.toString() === b.toString();
      keys = Object.keys(a);
      length = keys.length;
      if (length !== Object.keys(b).length)
        return false;
      for (i = length;i-- !== 0; )
        if (!Object.prototype.hasOwnProperty.call(b, keys[i]))
          return false;
      for (i = length;i-- !== 0; ) {
        var key = keys[i];
        if (!equal(a[key], b[key]))
          return false;
      }
      return true;
    }
    return a !== a && b !== b;
  };
});

// ../../node_modules/.bun/json-schema-traverse@1.0.0/node_modules/json-schema-traverse/index.js
var require_json_schema_traverse = __commonJS(function(exports, module) {
  var traverse = module.exports = function(schema, opts, cb) {
    if (typeof opts == "function") {
      cb = opts;
      opts = {};
    }
    cb = opts.cb || cb;
    var pre = typeof cb == "function" ? cb : cb.pre || function() {};
    var post = cb.post || function() {};
    _traverse(opts, pre, post, schema, "", schema);
  };
  traverse.keywords = {
    additionalItems: true,
    items: true,
    contains: true,
    additionalProperties: true,
    propertyNames: true,
    not: true,
    if: true,
    then: true,
    else: true
  };
  traverse.arrayKeywords = {
    items: true,
    allOf: true,
    anyOf: true,
    oneOf: true
  };
  traverse.propsKeywords = {
    $defs: true,
    definitions: true,
    properties: true,
    patternProperties: true,
    dependencies: true
  };
  traverse.skipKeywords = {
    default: true,
    enum: true,
    const: true,
    required: true,
    maximum: true,
    minimum: true,
    exclusiveMaximum: true,
    exclusiveMinimum: true,
    multipleOf: true,
    maxLength: true,
    minLength: true,
    pattern: true,
    format: true,
    maxItems: true,
    minItems: true,
    uniqueItems: true,
    maxProperties: true,
    minProperties: true
  };
  function _traverse(opts, pre, post, schema, jsonPtr, rootSchema, parentJsonPtr, parentKeyword, parentSchema, keyIndex) {
    if (schema && typeof schema == "object" && !Array.isArray(schema)) {
      pre(schema, jsonPtr, rootSchema, parentJsonPtr, parentKeyword, parentSchema, keyIndex);
      for (var key in schema) {
        var sch = schema[key];
        if (Array.isArray(sch)) {
          if (key in traverse.arrayKeywords) {
            for (var i = 0;i < sch.length; i++)
              _traverse(opts, pre, post, sch[i], jsonPtr + "/" + key + "/" + i, rootSchema, jsonPtr, key, schema, i);
          }
        } else if (key in traverse.propsKeywords) {
          if (sch && typeof sch == "object") {
            for (var prop in sch)
              _traverse(opts, pre, post, sch[prop], jsonPtr + "/" + key + "/" + escapeJsonPtr(prop), rootSchema, jsonPtr, key, schema, prop);
          }
        } else if (key in traverse.keywords || opts.allKeys && !(key in traverse.skipKeywords)) {
          _traverse(opts, pre, post, sch, jsonPtr + "/" + key, rootSchema, jsonPtr, key, schema);
        }
      }
      post(schema, jsonPtr, rootSchema, parentJsonPtr, parentKeyword, parentSchema, keyIndex);
    }
  }
  function escapeJsonPtr(str) {
    return str.replace(/~/g, "~0").replace(/\//g, "~1");
  }
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/compile/resolve.js
var require_resolve = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.getSchemaRefs = exports.resolveUrl = exports.normalizeId = exports._getFullPath = exports.getFullPath = exports.inlineRef = undefined;
  var util_1 = require_util();
  var equal = require_fast_deep_equal();
  var traverse = require_json_schema_traverse();
  var SIMPLE_INLINED = new Set([
    "type",
    "format",
    "pattern",
    "maxLength",
    "minLength",
    "maxProperties",
    "minProperties",
    "maxItems",
    "minItems",
    "maximum",
    "minimum",
    "uniqueItems",
    "multipleOf",
    "required",
    "enum",
    "const"
  ]);
  function inlineRef(schema, limit = true) {
    if (typeof schema == "boolean")
      return true;
    if (limit === true)
      return !hasRef(schema);
    if (!limit)
      return false;
    return countKeys(schema) <= limit;
  }
  exports.inlineRef = inlineRef;
  var REF_KEYWORDS = new Set([
    "$ref",
    "$recursiveRef",
    "$recursiveAnchor",
    "$dynamicRef",
    "$dynamicAnchor"
  ]);
  function hasRef(schema) {
    for (const key in schema) {
      if (REF_KEYWORDS.has(key))
        return true;
      const sch = schema[key];
      if (Array.isArray(sch) && sch.some(hasRef))
        return true;
      if (typeof sch == "object" && hasRef(sch))
        return true;
    }
    return false;
  }
  function countKeys(schema) {
    let count = 0;
    for (const key in schema) {
      if (key === "$ref")
        return Infinity;
      count++;
      if (SIMPLE_INLINED.has(key))
        continue;
      if (typeof schema[key] == "object") {
        (0, util_1.eachItem)(schema[key], (sch) => count += countKeys(sch));
      }
      if (count === Infinity)
        return Infinity;
    }
    return count;
  }
  function getFullPath(resolver, id = "", normalize) {
    if (normalize !== false)
      id = normalizeId(id);
    const p = resolver.parse(id);
    return _getFullPath(resolver, p);
  }
  exports.getFullPath = getFullPath;
  function _getFullPath(resolver, p) {
    const serialized = resolver.serialize(p);
    return serialized.split("#")[0] + "#";
  }
  exports._getFullPath = _getFullPath;
  var TRAILING_SLASH_HASH = /#\/?$/;
  function normalizeId(id) {
    return id ? id.replace(TRAILING_SLASH_HASH, "") : "";
  }
  exports.normalizeId = normalizeId;
  function resolveUrl(resolver, baseId, id) {
    id = normalizeId(id);
    return resolver.resolve(baseId, id);
  }
  exports.resolveUrl = resolveUrl;
  var ANCHOR = /^[a-z_][-a-z0-9._]*$/i;
  function getSchemaRefs(schema, baseId) {
    if (typeof schema == "boolean")
      return {};
    const { schemaId, uriResolver } = this.opts;
    const schId = normalizeId(schema[schemaId] || baseId);
    const baseIds = { "": schId };
    const pathPrefix = getFullPath(uriResolver, schId, false);
    const localRefs = {};
    const schemaRefs = new Set;
    traverse(schema, { allKeys: true }, (sch, jsonPtr, _, parentJsonPtr) => {
      if (parentJsonPtr === undefined)
        return;
      const fullPath = pathPrefix + jsonPtr;
      let innerBaseId = baseIds[parentJsonPtr];
      if (typeof sch[schemaId] == "string")
        innerBaseId = addRef.call(this, sch[schemaId]);
      addAnchor.call(this, sch.$anchor);
      addAnchor.call(this, sch.$dynamicAnchor);
      baseIds[jsonPtr] = innerBaseId;
      function addRef(ref) {
        const _resolve = this.opts.uriResolver.resolve;
        ref = normalizeId(innerBaseId ? _resolve(innerBaseId, ref) : ref);
        if (schemaRefs.has(ref))
          throw ambiguos(ref);
        schemaRefs.add(ref);
        let schOrRef = this.refs[ref];
        if (typeof schOrRef == "string")
          schOrRef = this.refs[schOrRef];
        if (typeof schOrRef == "object") {
          checkAmbiguosRef(sch, schOrRef.schema, ref);
        } else if (ref !== normalizeId(fullPath)) {
          if (ref[0] === "#") {
            checkAmbiguosRef(sch, localRefs[ref], ref);
            localRefs[ref] = sch;
          } else {
            this.refs[ref] = fullPath;
          }
        }
        return ref;
      }
      function addAnchor(anchor) {
        if (typeof anchor == "string") {
          if (!ANCHOR.test(anchor))
            throw new Error(`invalid anchor "${anchor}"`);
          addRef.call(this, `#${anchor}`);
        }
      }
    });
    return localRefs;
    function checkAmbiguosRef(sch1, sch2, ref) {
      if (sch2 !== undefined && !equal(sch1, sch2))
        throw ambiguos(ref);
    }
    function ambiguos(ref) {
      return new Error(`reference "${ref}" resolves to more than one schema`);
    }
  }
  exports.getSchemaRefs = getSchemaRefs;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/compile/validate/index.js
var require_validate = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.getData = exports.KeywordCxt = exports.validateFunctionCode = undefined;
  var boolSchema_1 = require_boolSchema();
  var dataType_1 = require_dataType();
  var applicability_1 = require_applicability();
  var dataType_2 = require_dataType();
  var defaults_1 = require_defaults();
  var keyword_1 = require_keyword();
  var subschema_1 = require_subschema();
  var codegen_1 = require_codegen();
  var names_1 = require_names();
  var resolve_1 = require_resolve();
  var util_1 = require_util();
  var errors_1 = require_errors();
  function validateFunctionCode(it) {
    if (isSchemaObj(it)) {
      checkKeywords(it);
      if (schemaCxtHasRules(it)) {
        topSchemaObjCode(it);
        return;
      }
    }
    validateFunction(it, () => (0, boolSchema_1.topBoolOrEmptySchema)(it));
  }
  exports.validateFunctionCode = validateFunctionCode;
  function validateFunction({ gen, validateName, schema, schemaEnv, opts }, body) {
    if (opts.code.es5) {
      gen.func(validateName, (0, codegen_1._)`${names_1.default.data}, ${names_1.default.valCxt}`, schemaEnv.$async, () => {
        gen.code((0, codegen_1._)`"use strict"; ${funcSourceUrl(schema, opts)}`);
        destructureValCxtES5(gen, opts);
        gen.code(body);
      });
    } else {
      gen.func(validateName, (0, codegen_1._)`${names_1.default.data}, ${destructureValCxt(opts)}`, schemaEnv.$async, () => gen.code(funcSourceUrl(schema, opts)).code(body));
    }
  }
  function destructureValCxt(opts) {
    return (0, codegen_1._)`{${names_1.default.instancePath}="", ${names_1.default.parentData}, ${names_1.default.parentDataProperty}, ${names_1.default.rootData}=${names_1.default.data}${opts.dynamicRef ? (0, codegen_1._)`, ${names_1.default.dynamicAnchors}={}` : codegen_1.nil}}={}`;
  }
  function destructureValCxtES5(gen, opts) {
    gen.if(names_1.default.valCxt, () => {
      gen.var(names_1.default.instancePath, (0, codegen_1._)`${names_1.default.valCxt}.${names_1.default.instancePath}`);
      gen.var(names_1.default.parentData, (0, codegen_1._)`${names_1.default.valCxt}.${names_1.default.parentData}`);
      gen.var(names_1.default.parentDataProperty, (0, codegen_1._)`${names_1.default.valCxt}.${names_1.default.parentDataProperty}`);
      gen.var(names_1.default.rootData, (0, codegen_1._)`${names_1.default.valCxt}.${names_1.default.rootData}`);
      if (opts.dynamicRef)
        gen.var(names_1.default.dynamicAnchors, (0, codegen_1._)`${names_1.default.valCxt}.${names_1.default.dynamicAnchors}`);
    }, () => {
      gen.var(names_1.default.instancePath, (0, codegen_1._)`""`);
      gen.var(names_1.default.parentData, (0, codegen_1._)`undefined`);
      gen.var(names_1.default.parentDataProperty, (0, codegen_1._)`undefined`);
      gen.var(names_1.default.rootData, names_1.default.data);
      if (opts.dynamicRef)
        gen.var(names_1.default.dynamicAnchors, (0, codegen_1._)`{}`);
    });
  }
  function topSchemaObjCode(it) {
    const { schema, opts, gen } = it;
    validateFunction(it, () => {
      if (opts.$comment && schema.$comment)
        commentKeyword(it);
      checkNoDefault(it);
      gen.let(names_1.default.vErrors, null);
      gen.let(names_1.default.errors, 0);
      if (opts.unevaluated)
        resetEvaluated(it);
      typeAndKeywords(it);
      returnResults(it);
    });
    return;
  }
  function resetEvaluated(it) {
    const { gen, validateName } = it;
    it.evaluated = gen.const("evaluated", (0, codegen_1._)`${validateName}.evaluated`);
    gen.if((0, codegen_1._)`${it.evaluated}.dynamicProps`, () => gen.assign((0, codegen_1._)`${it.evaluated}.props`, (0, codegen_1._)`undefined`));
    gen.if((0, codegen_1._)`${it.evaluated}.dynamicItems`, () => gen.assign((0, codegen_1._)`${it.evaluated}.items`, (0, codegen_1._)`undefined`));
  }
  function funcSourceUrl(schema, opts) {
    const schId = typeof schema == "object" && schema[opts.schemaId];
    return schId && (opts.code.source || opts.code.process) ? (0, codegen_1._)`/*# sourceURL=${schId} */` : codegen_1.nil;
  }
  function subschemaCode(it, valid) {
    if (isSchemaObj(it)) {
      checkKeywords(it);
      if (schemaCxtHasRules(it)) {
        subSchemaObjCode(it, valid);
        return;
      }
    }
    (0, boolSchema_1.boolOrEmptySchema)(it, valid);
  }
  function schemaCxtHasRules({ schema, self }) {
    if (typeof schema == "boolean")
      return !schema;
    for (const key in schema)
      if (self.RULES.all[key])
        return true;
    return false;
  }
  function isSchemaObj(it) {
    return typeof it.schema != "boolean";
  }
  function subSchemaObjCode(it, valid) {
    const { schema, gen, opts } = it;
    if (opts.$comment && schema.$comment)
      commentKeyword(it);
    updateContext(it);
    checkAsyncSchema(it);
    const errsCount = gen.const("_errs", names_1.default.errors);
    typeAndKeywords(it, errsCount);
    gen.var(valid, (0, codegen_1._)`${errsCount} === ${names_1.default.errors}`);
  }
  function checkKeywords(it) {
    (0, util_1.checkUnknownRules)(it);
    checkRefsAndKeywords(it);
  }
  function typeAndKeywords(it, errsCount) {
    if (it.opts.jtd)
      return schemaKeywords(it, [], false, errsCount);
    const types = (0, dataType_1.getSchemaTypes)(it.schema);
    const checkedTypes = (0, dataType_1.coerceAndCheckDataType)(it, types);
    schemaKeywords(it, types, !checkedTypes, errsCount);
  }
  function checkRefsAndKeywords(it) {
    const { schema, errSchemaPath, opts, self } = it;
    if (schema.$ref && opts.ignoreKeywordsWithRef && (0, util_1.schemaHasRulesButRef)(schema, self.RULES)) {
      self.logger.warn(`$ref: keywords ignored in schema at path "${errSchemaPath}"`);
    }
  }
  function checkNoDefault(it) {
    const { schema, opts } = it;
    if (schema.default !== undefined && opts.useDefaults && opts.strictSchema) {
      (0, util_1.checkStrictMode)(it, "default is ignored in the schema root");
    }
  }
  function updateContext(it) {
    const schId = it.schema[it.opts.schemaId];
    if (schId)
      it.baseId = (0, resolve_1.resolveUrl)(it.opts.uriResolver, it.baseId, schId);
  }
  function checkAsyncSchema(it) {
    if (it.schema.$async && !it.schemaEnv.$async)
      throw new Error("async schema in sync schema");
  }
  function commentKeyword({ gen, schemaEnv, schema, errSchemaPath, opts }) {
    const msg = schema.$comment;
    if (opts.$comment === true) {
      gen.code((0, codegen_1._)`${names_1.default.self}.logger.log(${msg})`);
    } else if (typeof opts.$comment == "function") {
      const schemaPath = (0, codegen_1.str)`${errSchemaPath}/$comment`;
      const rootName = gen.scopeValue("root", { ref: schemaEnv.root });
      gen.code((0, codegen_1._)`${names_1.default.self}.opts.$comment(${msg}, ${schemaPath}, ${rootName}.schema)`);
    }
  }
  function returnResults(it) {
    const { gen, schemaEnv, validateName, ValidationError, opts } = it;
    if (schemaEnv.$async) {
      gen.if((0, codegen_1._)`${names_1.default.errors} === 0`, () => gen.return(names_1.default.data), () => gen.throw((0, codegen_1._)`new ${ValidationError}(${names_1.default.vErrors})`));
    } else {
      gen.assign((0, codegen_1._)`${validateName}.errors`, names_1.default.vErrors);
      if (opts.unevaluated)
        assignEvaluated(it);
      gen.return((0, codegen_1._)`${names_1.default.errors} === 0`);
    }
  }
  function assignEvaluated({ gen, evaluated, props, items }) {
    if (props instanceof codegen_1.Name)
      gen.assign((0, codegen_1._)`${evaluated}.props`, props);
    if (items instanceof codegen_1.Name)
      gen.assign((0, codegen_1._)`${evaluated}.items`, items);
  }
  function schemaKeywords(it, types, typeErrors, errsCount) {
    const { gen, schema, data, allErrors, opts, self } = it;
    const { RULES } = self;
    if (schema.$ref && (opts.ignoreKeywordsWithRef || !(0, util_1.schemaHasRulesButRef)(schema, RULES))) {
      gen.block(() => keywordCode(it, "$ref", RULES.all.$ref.definition));
      return;
    }
    if (!opts.jtd)
      checkStrictTypes(it, types);
    gen.block(() => {
      for (const group of RULES.rules)
        groupKeywords(group);
      groupKeywords(RULES.post);
    });
    function groupKeywords(group) {
      if (!(0, applicability_1.shouldUseGroup)(schema, group))
        return;
      if (group.type) {
        gen.if((0, dataType_2.checkDataType)(group.type, data, opts.strictNumbers));
        iterateKeywords(it, group);
        if (types.length === 1 && types[0] === group.type && typeErrors) {
          gen.else();
          (0, dataType_2.reportTypeError)(it);
        }
        gen.endIf();
      } else {
        iterateKeywords(it, group);
      }
      if (!allErrors)
        gen.if((0, codegen_1._)`${names_1.default.errors} === ${errsCount || 0}`);
    }
  }
  function iterateKeywords(it, group) {
    const { gen, schema, opts: { useDefaults } } = it;
    if (useDefaults)
      (0, defaults_1.assignDefaults)(it, group.type);
    gen.block(() => {
      for (const rule of group.rules) {
        if ((0, applicability_1.shouldUseRule)(schema, rule)) {
          keywordCode(it, rule.keyword, rule.definition, group.type);
        }
      }
    });
  }
  function checkStrictTypes(it, types) {
    if (it.schemaEnv.meta || !it.opts.strictTypes)
      return;
    checkContextTypes(it, types);
    if (!it.opts.allowUnionTypes)
      checkMultipleTypes(it, types);
    checkKeywordTypes(it, it.dataTypes);
  }
  function checkContextTypes(it, types) {
    if (!types.length)
      return;
    if (!it.dataTypes.length) {
      it.dataTypes = types;
      return;
    }
    types.forEach((t) => {
      if (!includesType(it.dataTypes, t)) {
        strictTypesError(it, `type "${t}" not allowed by context "${it.dataTypes.join(",")}"`);
      }
    });
    narrowSchemaTypes(it, types);
  }
  function checkMultipleTypes(it, ts) {
    if (ts.length > 1 && !(ts.length === 2 && ts.includes("null"))) {
      strictTypesError(it, "use allowUnionTypes to allow union type keyword");
    }
  }
  function checkKeywordTypes(it, ts) {
    const rules = it.self.RULES.all;
    for (const keyword in rules) {
      const rule = rules[keyword];
      if (typeof rule == "object" && (0, applicability_1.shouldUseRule)(it.schema, rule)) {
        const { type } = rule.definition;
        if (type.length && !type.some((t) => hasApplicableType(ts, t))) {
          strictTypesError(it, `missing type "${type.join(",")}" for keyword "${keyword}"`);
        }
      }
    }
  }
  function hasApplicableType(schTs, kwdT) {
    return schTs.includes(kwdT) || kwdT === "number" && schTs.includes("integer");
  }
  function includesType(ts, t) {
    return ts.includes(t) || t === "integer" && ts.includes("number");
  }
  function narrowSchemaTypes(it, withTypes) {
    const ts = [];
    for (const t of it.dataTypes) {
      if (includesType(withTypes, t))
        ts.push(t);
      else if (withTypes.includes("integer") && t === "number")
        ts.push("integer");
    }
    it.dataTypes = ts;
  }
  function strictTypesError(it, msg) {
    const schemaPath = it.schemaEnv.baseId + it.errSchemaPath;
    msg += ` at "${schemaPath}" (strictTypes)`;
    (0, util_1.checkStrictMode)(it, msg, it.opts.strictTypes);
  }

  class KeywordCxt {
    constructor(it, def, keyword) {
      (0, keyword_1.validateKeywordUsage)(it, def, keyword);
      this.gen = it.gen;
      this.allErrors = it.allErrors;
      this.keyword = keyword;
      this.data = it.data;
      this.schema = it.schema[keyword];
      this.$data = def.$data && it.opts.$data && this.schema && this.schema.$data;
      this.schemaValue = (0, util_1.schemaRefOrVal)(it, this.schema, keyword, this.$data);
      this.schemaType = def.schemaType;
      this.parentSchema = it.schema;
      this.params = {};
      this.it = it;
      this.def = def;
      if (this.$data) {
        this.schemaCode = it.gen.const("vSchema", getData(this.$data, it));
      } else {
        this.schemaCode = this.schemaValue;
        if (!(0, keyword_1.validSchemaType)(this.schema, def.schemaType, def.allowUndefined)) {
          throw new Error(`${keyword} value must be ${JSON.stringify(def.schemaType)}`);
        }
      }
      if ("code" in def ? def.trackErrors : def.errors !== false) {
        this.errsCount = it.gen.const("_errs", names_1.default.errors);
      }
    }
    result(condition, successAction, failAction) {
      this.failResult((0, codegen_1.not)(condition), successAction, failAction);
    }
    failResult(condition, successAction, failAction) {
      this.gen.if(condition);
      if (failAction)
        failAction();
      else
        this.error();
      if (successAction) {
        this.gen.else();
        successAction();
        if (this.allErrors)
          this.gen.endIf();
      } else {
        if (this.allErrors)
          this.gen.endIf();
        else
          this.gen.else();
      }
    }
    pass(condition, failAction) {
      this.failResult((0, codegen_1.not)(condition), undefined, failAction);
    }
    fail(condition) {
      if (condition === undefined) {
        this.error();
        if (!this.allErrors)
          this.gen.if(false);
        return;
      }
      this.gen.if(condition);
      this.error();
      if (this.allErrors)
        this.gen.endIf();
      else
        this.gen.else();
    }
    fail$data(condition) {
      if (!this.$data)
        return this.fail(condition);
      const { schemaCode } = this;
      this.fail((0, codegen_1._)`${schemaCode} !== undefined && (${(0, codegen_1.or)(this.invalid$data(), condition)})`);
    }
    error(append, errorParams, errorPaths) {
      if (errorParams) {
        this.setParams(errorParams);
        this._error(append, errorPaths);
        this.setParams({});
        return;
      }
      this._error(append, errorPaths);
    }
    _error(append, errorPaths) {
      (append ? errors_1.reportExtraError : errors_1.reportError)(this, this.def.error, errorPaths);
    }
    $dataError() {
      (0, errors_1.reportError)(this, this.def.$dataError || errors_1.keyword$DataError);
    }
    reset() {
      if (this.errsCount === undefined)
        throw new Error('add "trackErrors" to keyword definition');
      (0, errors_1.resetErrorsCount)(this.gen, this.errsCount);
    }
    ok(cond) {
      if (!this.allErrors)
        this.gen.if(cond);
    }
    setParams(obj, assign) {
      if (assign)
        Object.assign(this.params, obj);
      else
        this.params = obj;
    }
    block$data(valid, codeBlock, $dataValid = codegen_1.nil) {
      this.gen.block(() => {
        this.check$data(valid, $dataValid);
        codeBlock();
      });
    }
    check$data(valid = codegen_1.nil, $dataValid = codegen_1.nil) {
      if (!this.$data)
        return;
      const { gen, schemaCode, schemaType, def } = this;
      gen.if((0, codegen_1.or)((0, codegen_1._)`${schemaCode} === undefined`, $dataValid));
      if (valid !== codegen_1.nil)
        gen.assign(valid, true);
      if (schemaType.length || def.validateSchema) {
        gen.elseIf(this.invalid$data());
        this.$dataError();
        if (valid !== codegen_1.nil)
          gen.assign(valid, false);
      }
      gen.else();
    }
    invalid$data() {
      const { gen, schemaCode, schemaType, def, it } = this;
      return (0, codegen_1.or)(wrong$DataType(), invalid$DataSchema());
      function wrong$DataType() {
        if (schemaType.length) {
          if (!(schemaCode instanceof codegen_1.Name))
            throw new Error("ajv implementation error");
          const st = Array.isArray(schemaType) ? schemaType : [schemaType];
          return (0, codegen_1._)`${(0, dataType_2.checkDataTypes)(st, schemaCode, it.opts.strictNumbers, dataType_2.DataType.Wrong)}`;
        }
        return codegen_1.nil;
      }
      function invalid$DataSchema() {
        if (def.validateSchema) {
          const validateSchemaRef = gen.scopeValue("validate$data", { ref: def.validateSchema });
          return (0, codegen_1._)`!${validateSchemaRef}(${schemaCode})`;
        }
        return codegen_1.nil;
      }
    }
    subschema(appl, valid) {
      const subschema = (0, subschema_1.getSubschema)(this.it, appl);
      (0, subschema_1.extendSubschemaData)(subschema, this.it, appl);
      (0, subschema_1.extendSubschemaMode)(subschema, appl);
      const nextContext = { ...this.it, ...subschema, items: undefined, props: undefined };
      subschemaCode(nextContext, valid);
      return nextContext;
    }
    mergeEvaluated(schemaCxt, toName) {
      const { it, gen } = this;
      if (!it.opts.unevaluated)
        return;
      if (it.props !== true && schemaCxt.props !== undefined) {
        it.props = util_1.mergeEvaluated.props(gen, schemaCxt.props, it.props, toName);
      }
      if (it.items !== true && schemaCxt.items !== undefined) {
        it.items = util_1.mergeEvaluated.items(gen, schemaCxt.items, it.items, toName);
      }
    }
    mergeValidEvaluated(schemaCxt, valid) {
      const { it, gen } = this;
      if (it.opts.unevaluated && (it.props !== true || it.items !== true)) {
        gen.if(valid, () => this.mergeEvaluated(schemaCxt, codegen_1.Name));
        return true;
      }
    }
  }
  exports.KeywordCxt = KeywordCxt;
  function keywordCode(it, keyword, def, ruleType) {
    const cxt = new KeywordCxt(it, def, keyword);
    if ("code" in def) {
      def.code(cxt, ruleType);
    } else if (cxt.$data && def.validate) {
      (0, keyword_1.funcKeywordCode)(cxt, def);
    } else if ("macro" in def) {
      (0, keyword_1.macroKeywordCode)(cxt, def);
    } else if (def.compile || def.validate) {
      (0, keyword_1.funcKeywordCode)(cxt, def);
    }
  }
  var JSON_POINTER = /^\/(?:[^~]|~0|~1)*$/;
  var RELATIVE_JSON_POINTER = /^([0-9]+)(#|\/(?:[^~]|~0|~1)*)?$/;
  function getData($data, { dataLevel, dataNames, dataPathArr }) {
    let jsonPointer;
    let data;
    if ($data === "")
      return names_1.default.rootData;
    if ($data[0] === "/") {
      if (!JSON_POINTER.test($data))
        throw new Error(`Invalid JSON-pointer: ${$data}`);
      jsonPointer = $data;
      data = names_1.default.rootData;
    } else {
      const matches = RELATIVE_JSON_POINTER.exec($data);
      if (!matches)
        throw new Error(`Invalid JSON-pointer: ${$data}`);
      const up = +matches[1];
      jsonPointer = matches[2];
      if (jsonPointer === "#") {
        if (up >= dataLevel)
          throw new Error(errorMsg("property/index", up));
        return dataPathArr[dataLevel - up];
      }
      if (up > dataLevel)
        throw new Error(errorMsg("data", up));
      data = dataNames[dataLevel - up];
      if (!jsonPointer)
        return data;
    }
    let expr = data;
    const segments = jsonPointer.split("/");
    for (const segment of segments) {
      if (segment) {
        data = (0, codegen_1._)`${data}${(0, codegen_1.getProperty)((0, util_1.unescapeJsonPointer)(segment))}`;
        expr = (0, codegen_1._)`${expr} && ${data}`;
      }
    }
    return expr;
    function errorMsg(pointerType, up) {
      return `Cannot access ${pointerType} ${up} levels up, current level is ${dataLevel}`;
    }
  }
  exports.getData = getData;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/runtime/validation_error.js
var require_validation_error = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });

  class ValidationError extends Error {
    constructor(errors) {
      super("validation failed");
      this.errors = errors;
      this.ajv = this.validation = true;
    }
  }
  exports.default = ValidationError;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/compile/ref_error.js
var require_ref_error = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var resolve_1 = require_resolve();

  class MissingRefError extends Error {
    constructor(resolver, baseId, ref, msg) {
      super(msg || `can't resolve reference ${ref} from id ${baseId}`);
      this.missingRef = (0, resolve_1.resolveUrl)(resolver, baseId, ref);
      this.missingSchema = (0, resolve_1.normalizeId)((0, resolve_1.getFullPath)(resolver, this.missingRef));
    }
  }
  exports.default = MissingRefError;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/compile/index.js
var require_compile = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.resolveSchema = exports.getCompilingSchema = exports.resolveRef = exports.compileSchema = exports.SchemaEnv = undefined;
  var codegen_1 = require_codegen();
  var validation_error_1 = require_validation_error();
  var names_1 = require_names();
  var resolve_1 = require_resolve();
  var util_1 = require_util();
  var validate_1 = require_validate();

  class SchemaEnv {
    constructor(env) {
      var _a;
      this.refs = {};
      this.dynamicAnchors = {};
      let schema;
      if (typeof env.schema == "object")
        schema = env.schema;
      this.schema = env.schema;
      this.schemaId = env.schemaId;
      this.root = env.root || this;
      this.baseId = (_a = env.baseId) !== null && _a !== undefined ? _a : (0, resolve_1.normalizeId)(schema === null || schema === undefined ? undefined : schema[env.schemaId || "$id"]);
      this.schemaPath = env.schemaPath;
      this.localRefs = env.localRefs;
      this.meta = env.meta;
      this.$async = schema === null || schema === undefined ? undefined : schema.$async;
      this.refs = {};
    }
  }
  exports.SchemaEnv = SchemaEnv;
  function compileSchema(sch) {
    const _sch = getCompilingSchema.call(this, sch);
    if (_sch)
      return _sch;
    const rootId = (0, resolve_1.getFullPath)(this.opts.uriResolver, sch.root.baseId);
    const { es5, lines } = this.opts.code;
    const { ownProperties } = this.opts;
    const gen = new codegen_1.CodeGen(this.scope, { es5, lines, ownProperties });
    let _ValidationError;
    if (sch.$async) {
      _ValidationError = gen.scopeValue("Error", {
        ref: validation_error_1.default,
        code: (0, codegen_1._)`require("ajv/dist/runtime/validation_error").default`
      });
    }
    const validateName = gen.scopeName("validate");
    sch.validateName = validateName;
    const schemaCxt = {
      gen,
      allErrors: this.opts.allErrors,
      data: names_1.default.data,
      parentData: names_1.default.parentData,
      parentDataProperty: names_1.default.parentDataProperty,
      dataNames: [names_1.default.data],
      dataPathArr: [codegen_1.nil],
      dataLevel: 0,
      dataTypes: [],
      definedProperties: new Set,
      topSchemaRef: gen.scopeValue("schema", this.opts.code.source === true ? { ref: sch.schema, code: (0, codegen_1.stringify)(sch.schema) } : { ref: sch.schema }),
      validateName,
      ValidationError: _ValidationError,
      schema: sch.schema,
      schemaEnv: sch,
      rootId,
      baseId: sch.baseId || rootId,
      schemaPath: codegen_1.nil,
      errSchemaPath: sch.schemaPath || (this.opts.jtd ? "" : "#"),
      errorPath: (0, codegen_1._)`""`,
      opts: this.opts,
      self: this
    };
    let sourceCode;
    try {
      this._compilations.add(sch);
      (0, validate_1.validateFunctionCode)(schemaCxt);
      gen.optimize(this.opts.code.optimize);
      const validateCode = gen.toString();
      sourceCode = `${gen.scopeRefs(names_1.default.scope)}return ${validateCode}`;
      if (this.opts.code.process)
        sourceCode = this.opts.code.process(sourceCode, sch);
      const makeValidate = new Function(`${names_1.default.self}`, `${names_1.default.scope}`, sourceCode);
      const validate = makeValidate(this, this.scope.get());
      this.scope.value(validateName, { ref: validate });
      validate.errors = null;
      validate.schema = sch.schema;
      validate.schemaEnv = sch;
      if (sch.$async)
        validate.$async = true;
      if (this.opts.code.source === true) {
        validate.source = { validateName, validateCode, scopeValues: gen._values };
      }
      if (this.opts.unevaluated) {
        const { props, items } = schemaCxt;
        validate.evaluated = {
          props: props instanceof codegen_1.Name ? undefined : props,
          items: items instanceof codegen_1.Name ? undefined : items,
          dynamicProps: props instanceof codegen_1.Name,
          dynamicItems: items instanceof codegen_1.Name
        };
        if (validate.source)
          validate.source.evaluated = (0, codegen_1.stringify)(validate.evaluated);
      }
      sch.validate = validate;
      return sch;
    } catch (e) {
      delete sch.validate;
      delete sch.validateName;
      if (sourceCode)
        this.logger.error("Error compiling schema, function code:", sourceCode);
      throw e;
    } finally {
      this._compilations.delete(sch);
    }
  }
  exports.compileSchema = compileSchema;
  function resolveRef(root, baseId, ref) {
    var _a;
    ref = (0, resolve_1.resolveUrl)(this.opts.uriResolver, baseId, ref);
    const schOrFunc = root.refs[ref];
    if (schOrFunc)
      return schOrFunc;
    let _sch = resolve.call(this, root, ref);
    if (_sch === undefined) {
      const schema = (_a = root.localRefs) === null || _a === undefined ? undefined : _a[ref];
      const { schemaId } = this.opts;
      if (schema)
        _sch = new SchemaEnv({ schema, schemaId, root, baseId });
    }
    if (_sch === undefined)
      return;
    return root.refs[ref] = inlineOrCompile.call(this, _sch);
  }
  exports.resolveRef = resolveRef;
  function inlineOrCompile(sch) {
    if ((0, resolve_1.inlineRef)(sch.schema, this.opts.inlineRefs))
      return sch.schema;
    return sch.validate ? sch : compileSchema.call(this, sch);
  }
  function getCompilingSchema(schEnv) {
    for (const sch of this._compilations) {
      if (sameSchemaEnv(sch, schEnv))
        return sch;
    }
  }
  exports.getCompilingSchema = getCompilingSchema;
  function sameSchemaEnv(s1, s2) {
    return s1.schema === s2.schema && s1.root === s2.root && s1.baseId === s2.baseId;
  }
  function resolve(root, ref) {
    let sch;
    while (typeof (sch = this.refs[ref]) == "string")
      ref = sch;
    return sch || this.schemas[ref] || resolveSchema.call(this, root, ref);
  }
  function resolveSchema(root, ref) {
    const p = this.opts.uriResolver.parse(ref);
    const refPath = (0, resolve_1._getFullPath)(this.opts.uriResolver, p);
    let baseId = (0, resolve_1.getFullPath)(this.opts.uriResolver, root.baseId, undefined);
    if (Object.keys(root.schema).length > 0 && refPath === baseId) {
      return getJsonPointer.call(this, p, root);
    }
    const id = (0, resolve_1.normalizeId)(refPath);
    const schOrRef = this.refs[id] || this.schemas[id];
    if (typeof schOrRef == "string") {
      const sch = resolveSchema.call(this, root, schOrRef);
      if (typeof (sch === null || sch === undefined ? undefined : sch.schema) !== "object")
        return;
      return getJsonPointer.call(this, p, sch);
    }
    if (typeof (schOrRef === null || schOrRef === undefined ? undefined : schOrRef.schema) !== "object")
      return;
    if (!schOrRef.validate)
      compileSchema.call(this, schOrRef);
    if (id === (0, resolve_1.normalizeId)(ref)) {
      const { schema } = schOrRef;
      const { schemaId } = this.opts;
      const schId = schema[schemaId];
      if (schId)
        baseId = (0, resolve_1.resolveUrl)(this.opts.uriResolver, baseId, schId);
      return new SchemaEnv({ schema, schemaId, root, baseId });
    }
    return getJsonPointer.call(this, p, schOrRef);
  }
  exports.resolveSchema = resolveSchema;
  var PREVENT_SCOPE_CHANGE = new Set([
    "properties",
    "patternProperties",
    "enum",
    "dependencies",
    "definitions"
  ]);
  function getJsonPointer(parsedRef, { baseId, schema, root }) {
    var _a;
    if (((_a = parsedRef.fragment) === null || _a === undefined ? undefined : _a[0]) !== "/")
      return;
    for (const part of parsedRef.fragment.slice(1).split("/")) {
      if (typeof schema === "boolean")
        return;
      const partSchema = schema[(0, util_1.unescapeFragment)(part)];
      if (partSchema === undefined)
        return;
      schema = partSchema;
      const schId = typeof schema === "object" && schema[this.opts.schemaId];
      if (!PREVENT_SCOPE_CHANGE.has(part) && schId) {
        baseId = (0, resolve_1.resolveUrl)(this.opts.uriResolver, baseId, schId);
      }
    }
    let env;
    if (typeof schema != "boolean" && schema.$ref && !(0, util_1.schemaHasRulesButRef)(schema, this.RULES)) {
      const $ref = (0, resolve_1.resolveUrl)(this.opts.uriResolver, baseId, schema.$ref);
      env = resolveSchema.call(this, root, $ref);
    }
    const { schemaId } = this.opts;
    env = env || new SchemaEnv({ schema, schemaId, root, baseId });
    if (env.schema !== env.root.schema)
      return env;
    return;
  }
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/refs/data.json
var require_data = __commonJS(function(exports, module) {
  module.exports = {
    $id: "https://raw.githubusercontent.com/ajv-validator/ajv/master/lib/refs/data.json#",
    description: "Meta-schema for $data reference (JSON AnySchema extension proposal)",
    type: "object",
    required: ["$data"],
    properties: {
      $data: {
        type: "string",
        anyOf: [{ format: "relative-json-pointer" }, { format: "json-pointer" }]
      }
    },
    additionalProperties: false
  };
});

// ../../node_modules/.bun/fast-uri@3.1.5/node_modules/fast-uri/lib/utils.js
var require_utils = __commonJS(function(exports, module) {
  var isUUID = RegExp.prototype.test.bind(/^[\da-f]{8}-[\da-f]{4}-[\da-f]{4}-[\da-f]{4}-[\da-f]{12}$/iu);
  var isIPv4 = RegExp.prototype.test.bind(/^(?:(?:25[0-5]|2[0-4]\d|1\d{2}|[1-9]\d|\d)\.){3}(?:25[0-5]|2[0-4]\d|1\d{2}|[1-9]\d|\d)$/u);
  var isHexPair = RegExp.prototype.test.bind(/^[\da-f]{2}$/iu);
  var isUnreserved = RegExp.prototype.test.bind(/^[\da-z\-._~]$/iu);
  var isPathCharacter = RegExp.prototype.test.bind(/^[\da-z\-._~!$&'()*+,;=:@/]$/iu);
  function stringArrayToHexStripped(input) {
    let acc = "";
    let code = 0;
    let i = 0;
    for (i = 0;i < input.length; i++) {
      code = input[i].charCodeAt(0);
      if (code === 48) {
        continue;
      }
      if (!(code >= 48 && code <= 57 || code >= 65 && code <= 70 || code >= 97 && code <= 102)) {
        return "";
      }
      acc += input[i];
      break;
    }
    for (i += 1;i < input.length; i++) {
      code = input[i].charCodeAt(0);
      if (!(code >= 48 && code <= 57 || code >= 65 && code <= 70 || code >= 97 && code <= 102)) {
        return "";
      }
      acc += input[i];
    }
    return acc;
  }
  var nonSimpleDomain = RegExp.prototype.test.bind(/[^!"$&'()*+,\-.;=_`a-z{}~]/u);
  function consumeIsZone(buffer) {
    buffer.length = 0;
    return true;
  }
  function consumeHextets(buffer, address, output) {
    if (buffer.length) {
      const hex = stringArrayToHexStripped(buffer);
      if (hex !== "") {
        address.push(hex);
      } else {
        output.error = true;
        return false;
      }
      buffer.length = 0;
    }
    return true;
  }
  function getIPV6(input) {
    let tokenCount = 0;
    const output = { error: false, address: "", zone: "" };
    const address = [];
    const buffer = [];
    let endipv6Encountered = false;
    let endIpv6 = false;
    let consume = consumeHextets;
    for (let i = 0;i < input.length; i++) {
      const cursor = input[i];
      if (cursor === "[" || cursor === "]") {
        continue;
      }
      if (cursor === ":") {
        if (endipv6Encountered === true) {
          endIpv6 = true;
        }
        if (!consume(buffer, address, output)) {
          break;
        }
        if (++tokenCount > 7) {
          output.error = true;
          break;
        }
        if (i > 0 && input[i - 1] === ":") {
          endipv6Encountered = true;
        }
        address.push(":");
        continue;
      } else if (cursor === "%") {
        if (!consume(buffer, address, output)) {
          break;
        }
        consume = consumeIsZone;
      } else {
        buffer.push(cursor);
        continue;
      }
    }
    if (buffer.length) {
      if (consume === consumeIsZone) {
        output.zone = buffer.join("");
      } else if (endIpv6) {
        address.push(buffer.join(""));
      } else {
        address.push(stringArrayToHexStripped(buffer));
      }
    }
    output.address = address.join("");
    return output;
  }
  function normalizeIPv6(host) {
    if (findToken(host, ":") < 2) {
      return { host, isIPV6: false };
    }
    const ipv6 = getIPV6(host);
    if (!ipv6.error) {
      let newHost = ipv6.address;
      let escapedHost = ipv6.address;
      if (ipv6.zone) {
        newHost += "%" + ipv6.zone;
        escapedHost += "%25" + ipv6.zone;
      }
      return { host: newHost, isIPV6: true, escapedHost };
    } else {
      return { host, isIPV6: false };
    }
  }
  function findToken(str, token) {
    let ind = 0;
    for (let i = 0;i < str.length; i++) {
      if (str[i] === token)
        ind++;
    }
    return ind;
  }
  function removeDotSegments(path) {
    let input = path;
    const output = [];
    let nextSlash = -1;
    let len = 0;
    while (len = input.length) {
      if (len === 1) {
        if (input === ".") {
          break;
        } else if (input === "/") {
          output.push("/");
          break;
        } else {
          output.push(input);
          break;
        }
      } else if (len === 2) {
        if (input[0] === ".") {
          if (input[1] === ".") {
            break;
          } else if (input[1] === "/") {
            input = input.slice(2);
            continue;
          }
        } else if (input[0] === "/") {
          if (input[1] === "." || input[1] === "/") {
            output.push("/");
            break;
          }
        }
      } else if (len === 3) {
        if (input === "/..") {
          if (output.length !== 0) {
            output.pop();
          }
          output.push("/");
          break;
        }
      }
      if (input[0] === ".") {
        if (input[1] === ".") {
          if (input[2] === "/") {
            input = input.slice(3);
            continue;
          }
        } else if (input[1] === "/") {
          input = input.slice(2);
          continue;
        }
      } else if (input[0] === "/") {
        if (input[1] === ".") {
          if (input[2] === "/") {
            input = input.slice(2);
            continue;
          } else if (input[2] === ".") {
            if (input[3] === "/") {
              input = input.slice(3);
              if (output.length !== 0) {
                output.pop();
              }
              continue;
            }
          }
        }
      }
      if ((nextSlash = input.indexOf("/", 1)) === -1) {
        output.push(input);
        break;
      } else {
        output.push(input.slice(0, nextSlash));
        input = input.slice(nextSlash);
      }
    }
    return output.join("");
  }
  var HOST_DELIMS = { "@": "%40", "/": "%2F", "?": "%3F", "#": "%23", ":": "%3A" };
  var HOST_DELIM_RE = /[@/?#:]/g;
  var HOST_DELIM_NO_COLON_RE = /[@/?#]/g;
  function reescapeHostDelimiters(host, isIP) {
    const re = isIP ? HOST_DELIM_NO_COLON_RE : HOST_DELIM_RE;
    re.lastIndex = 0;
    return host.replace(re, (ch) => HOST_DELIMS[ch]);
  }
  function normalizePercentEncoding(input, decodeUnreserved = false) {
    if (input.indexOf("%") === -1) {
      return input;
    }
    let output = "";
    for (let i = 0;i < input.length; i++) {
      if (input[i] === "%" && i + 2 < input.length) {
        const hex = input.slice(i + 1, i + 3);
        if (isHexPair(hex)) {
          const normalizedHex = hex.toUpperCase();
          const decoded = String.fromCharCode(parseInt(normalizedHex, 16));
          if (decodeUnreserved && isUnreserved(decoded)) {
            output += decoded;
          } else {
            output += "%" + normalizedHex;
          }
          i += 2;
          continue;
        }
      }
      output += input[i];
    }
    return output;
  }
  function normalizePathEncoding(input) {
    let output = "";
    for (let i = 0;i < input.length; i++) {
      if (input[i] === "%" && i + 2 < input.length) {
        const hex = input.slice(i + 1, i + 3);
        if (isHexPair(hex)) {
          const normalizedHex = hex.toUpperCase();
          const decoded = String.fromCharCode(parseInt(normalizedHex, 16));
          if (decoded !== "." && isUnreserved(decoded)) {
            output += decoded;
          } else {
            output += "%" + normalizedHex;
          }
          i += 2;
          continue;
        }
      }
      if (isPathCharacter(input[i])) {
        output += input[i];
      } else {
        output += escape(input[i]);
      }
    }
    return output;
  }
  function escapePreservingEscapes(input) {
    let output = "";
    for (let i = 0;i < input.length; i++) {
      if (input[i] === "%" && i + 2 < input.length) {
        const hex = input.slice(i + 1, i + 3);
        if (isHexPair(hex)) {
          output += "%" + hex.toUpperCase();
          i += 2;
          continue;
        }
      }
      output += escape(input[i]);
    }
    return output;
  }
  function recomposeAuthority(component) {
    const uriTokens = [];
    if (component.userinfo !== undefined) {
      uriTokens.push(component.userinfo);
      uriTokens.push("@");
    }
    if (component.host !== undefined) {
      let host = unescape(component.host);
      if (!isIPv4(host)) {
        const ipV6res = normalizeIPv6(host);
        if (ipV6res.isIPV6 === true) {
          host = `[${ipV6res.escapedHost}]`;
        } else {
          host = reescapeHostDelimiters(host, false);
        }
      }
      uriTokens.push(host);
    }
    if (typeof component.port === "number" || typeof component.port === "string") {
      uriTokens.push(":");
      uriTokens.push(String(component.port));
    }
    return uriTokens.length ? uriTokens.join("") : undefined;
  }
  module.exports = {
    nonSimpleDomain,
    recomposeAuthority,
    reescapeHostDelimiters,
    normalizePercentEncoding,
    normalizePathEncoding,
    escapePreservingEscapes,
    removeDotSegments,
    isIPv4,
    isUUID,
    normalizeIPv6,
    stringArrayToHexStripped
  };
});

// ../../node_modules/.bun/fast-uri@3.1.5/node_modules/fast-uri/lib/schemes.js
var require_schemes = __commonJS(function(exports, module) {
  var { isUUID } = require_utils();
  var URN_REG = /([\da-z][\d\-a-z]{0,31}):((?:[\w!$'()*+,\-.:;=@]|%[\da-f]{2})+)/iu;
  var supportedSchemeNames = [
    "http",
    "https",
    "ws",
    "wss",
    "urn",
    "urn:uuid"
  ];
  function isValidSchemeName(name) {
    return supportedSchemeNames.indexOf(name) !== -1;
  }
  function wsIsSecure(wsComponent) {
    if (wsComponent.secure === true) {
      return true;
    } else if (wsComponent.secure === false) {
      return false;
    } else if (wsComponent.scheme) {
      return wsComponent.scheme.length === 3 && (wsComponent.scheme[0] === "w" || wsComponent.scheme[0] === "W") && (wsComponent.scheme[1] === "s" || wsComponent.scheme[1] === "S") && (wsComponent.scheme[2] === "s" || wsComponent.scheme[2] === "S");
    } else {
      return false;
    }
  }
  function httpParse(component) {
    if (!component.host) {
      component.error = component.error || "HTTP URIs must have a host.";
    }
    return component;
  }
  function httpSerialize(component) {
    const secure = String(component.scheme).toLowerCase() === "https";
    if (component.port === (secure ? 443 : 80) || component.port === "") {
      component.port = undefined;
    }
    if (!component.path) {
      component.path = "/";
    }
    return component;
  }
  function wsParse(wsComponent) {
    wsComponent.secure = wsIsSecure(wsComponent);
    wsComponent.resourceName = (wsComponent.path || "/") + (wsComponent.query ? "?" + wsComponent.query : "");
    wsComponent.path = undefined;
    wsComponent.query = undefined;
    return wsComponent;
  }
  function wsSerialize(wsComponent) {
    if (wsComponent.port === (wsIsSecure(wsComponent) ? 443 : 80) || wsComponent.port === "") {
      wsComponent.port = undefined;
    }
    if (typeof wsComponent.secure === "boolean") {
      wsComponent.scheme = wsComponent.secure ? "wss" : "ws";
      wsComponent.secure = undefined;
    }
    if (wsComponent.resourceName) {
      const [path, query] = wsComponent.resourceName.split("?");
      wsComponent.path = path && path !== "/" ? path : undefined;
      wsComponent.query = query;
      wsComponent.resourceName = undefined;
    }
    wsComponent.fragment = undefined;
    return wsComponent;
  }
  function urnParse(urnComponent, options) {
    if (!urnComponent.path) {
      urnComponent.error = "URN can not be parsed";
      return urnComponent;
    }
    const matches = urnComponent.path.match(URN_REG);
    if (matches) {
      const scheme = options.scheme || urnComponent.scheme || "urn";
      urnComponent.nid = matches[1].toLowerCase();
      urnComponent.nss = matches[2];
      const urnScheme = `${scheme}:${options.nid || urnComponent.nid}`;
      const schemeHandler = getSchemeHandler(urnScheme);
      urnComponent.path = undefined;
      if (schemeHandler) {
        urnComponent = schemeHandler.parse(urnComponent, options);
      }
    } else {
      urnComponent.error = urnComponent.error || "URN can not be parsed.";
    }
    return urnComponent;
  }
  function urnSerialize(urnComponent, options) {
    if (urnComponent.nid === undefined) {
      throw new Error("URN without nid cannot be serialized");
    }
    const scheme = options.scheme || urnComponent.scheme || "urn";
    const nid = urnComponent.nid.toLowerCase();
    const urnScheme = `${scheme}:${options.nid || nid}`;
    const schemeHandler = getSchemeHandler(urnScheme);
    if (schemeHandler) {
      urnComponent = schemeHandler.serialize(urnComponent, options);
    }
    const uriComponent = urnComponent;
    const nss = urnComponent.nss;
    uriComponent.path = `${nid || options.nid}:${nss}`;
    options.skipEscape = true;
    return uriComponent;
  }
  function urnuuidParse(urnComponent, options) {
    const uuidComponent = urnComponent;
    uuidComponent.uuid = uuidComponent.nss;
    uuidComponent.nss = undefined;
    if (!options.tolerant && (!uuidComponent.uuid || !isUUID(uuidComponent.uuid))) {
      uuidComponent.error = uuidComponent.error || "UUID is not valid.";
    }
    return uuidComponent;
  }
  function urnuuidSerialize(uuidComponent) {
    const urnComponent = uuidComponent;
    urnComponent.nss = (uuidComponent.uuid || "").toLowerCase();
    return urnComponent;
  }
  var http = {
    scheme: "http",
    domainHost: true,
    parse: httpParse,
    serialize: httpSerialize
  };
  var https = {
    scheme: "https",
    domainHost: http.domainHost,
    parse: httpParse,
    serialize: httpSerialize
  };
  var ws = {
    scheme: "ws",
    domainHost: true,
    parse: wsParse,
    serialize: wsSerialize
  };
  var wss = {
    scheme: "wss",
    domainHost: ws.domainHost,
    parse: ws.parse,
    serialize: ws.serialize
  };
  var urn = {
    scheme: "urn",
    parse: urnParse,
    serialize: urnSerialize,
    skipNormalize: true
  };
  var urnuuid = {
    scheme: "urn:uuid",
    parse: urnuuidParse,
    serialize: urnuuidSerialize,
    skipNormalize: true
  };
  var SCHEMES = {
    http,
    https,
    ws,
    wss,
    urn,
    "urn:uuid": urnuuid
  };
  Object.setPrototypeOf(SCHEMES, null);
  function getSchemeHandler(scheme) {
    return scheme && (SCHEMES[scheme] || SCHEMES[scheme.toLowerCase()]) || undefined;
  }
  module.exports = {
    wsIsSecure,
    SCHEMES,
    isValidSchemeName,
    getSchemeHandler
  };
});

// ../../node_modules/.bun/fast-uri@3.1.5/node_modules/fast-uri/index.js
var require_fast_uri = __commonJS(function(exports, module) {
  var { normalizeIPv6, removeDotSegments, recomposeAuthority, normalizePercentEncoding, normalizePathEncoding, escapePreservingEscapes, reescapeHostDelimiters, isIPv4, nonSimpleDomain } = require_utils();
  var { SCHEMES, getSchemeHandler } = require_schemes();
  function normalize(uri, options) {
    if (typeof uri === "string") {
      uri = normalizeString(uri, options);
    } else if (typeof uri === "object") {
      uri = parse(serialize(uri, options), options);
    }
    return uri;
  }
  function resolve(baseURI, relativeURI, options) {
    const schemelessOptions = options ? Object.assign({ scheme: "null" }, options) : { scheme: "null" };
    const { parsed: baseParsed, malformedAuthorityOrPort: baseMalformed } = parseWithStatus(baseURI, schemelessOptions);
    const { parsed: relativeParsed, malformedAuthorityOrPort: relativeMalformed } = parseWithStatus(relativeURI, schemelessOptions);
    if (baseMalformed || relativeMalformed) {
      throw new Error(baseParsed.error || relativeParsed.error || "URI is malformed.");
    }
    const resolved = resolveComponent(baseParsed, relativeParsed, schemelessOptions, true);
    schemelessOptions.skipEscape = true;
    return serialize(resolved, schemelessOptions);
  }
  function resolveComponent(base, relative, options, skipNormalization) {
    const target = {};
    if (!skipNormalization) {
      base = parse(serialize(base, options), options);
      relative = parse(serialize(relative, options), options);
    }
    options = options || {};
    if (!options.tolerant && relative.scheme) {
      target.scheme = relative.scheme;
      target.userinfo = relative.userinfo;
      target.host = relative.host;
      target.port = relative.port;
      target.path = removeDotSegments(relative.path || "");
      target.query = relative.query;
    } else {
      if (relative.userinfo !== undefined || relative.host !== undefined || relative.port !== undefined) {
        target.userinfo = relative.userinfo;
        target.host = relative.host;
        target.port = relative.port;
        target.path = removeDotSegments(relative.path || "");
        target.query = relative.query;
      } else {
        if (!relative.path) {
          target.path = base.path;
          if (relative.query !== undefined) {
            target.query = relative.query;
          } else {
            target.query = base.query;
          }
        } else {
          if (relative.path[0] === "/") {
            target.path = removeDotSegments(relative.path);
          } else {
            if ((base.userinfo !== undefined || base.host !== undefined || base.port !== undefined) && !base.path) {
              target.path = "/" + relative.path;
            } else if (!base.path) {
              target.path = relative.path;
            } else {
              target.path = base.path.slice(0, base.path.lastIndexOf("/") + 1) + relative.path;
            }
            target.path = removeDotSegments(target.path);
          }
          target.query = relative.query;
        }
        target.userinfo = base.userinfo;
        target.host = base.host;
        target.port = base.port;
      }
      target.scheme = base.scheme;
    }
    target.fragment = relative.fragment;
    return target;
  }
  function equal(uriA, uriB, options) {
    const normalizedA = normalizeComparableURI(uriA, options);
    const normalizedB = normalizeComparableURI(uriB, options);
    return normalizedA !== undefined && normalizedB !== undefined && normalizedA.toLowerCase() === normalizedB.toLowerCase();
  }
  function serialize(cmpts, opts) {
    const component = {
      host: cmpts.host,
      scheme: cmpts.scheme,
      userinfo: cmpts.userinfo,
      port: cmpts.port,
      path: cmpts.path,
      query: cmpts.query,
      nid: cmpts.nid,
      nss: cmpts.nss,
      uuid: cmpts.uuid,
      fragment: cmpts.fragment,
      reference: cmpts.reference,
      resourceName: cmpts.resourceName,
      secure: cmpts.secure,
      error: ""
    };
    const options = Object.assign({}, opts);
    const uriTokens = [];
    const schemeHandler = getSchemeHandler(options.scheme || component.scheme);
    if (schemeHandler && schemeHandler.serialize)
      schemeHandler.serialize(component, options);
    if (component.path !== undefined) {
      if (!options.skipEscape) {
        component.path = escapePreservingEscapes(component.path);
        if (component.scheme !== undefined) {
          component.path = component.path.split("%3A").join(":");
        }
      } else {
        component.path = normalizePercentEncoding(component.path);
      }
    }
    if (options.reference !== "suffix" && component.scheme) {
      uriTokens.push(component.scheme, ":");
    }
    const authority = recomposeAuthority(component);
    if (authority !== undefined) {
      if (options.reference !== "suffix") {
        uriTokens.push("//");
      }
      uriTokens.push(authority);
      if (component.path && component.path[0] !== "/") {
        uriTokens.push("/");
      }
    }
    if (component.path !== undefined) {
      let s = component.path;
      if (!options.absolutePath && (!schemeHandler || !schemeHandler.absolutePath)) {
        s = removeDotSegments(s);
      }
      if (authority === undefined && s[0] === "/" && s[1] === "/") {
        s = "/%2F" + s.slice(2);
      }
      uriTokens.push(s);
    }
    if (component.query !== undefined) {
      uriTokens.push("?", component.query);
    }
    if (component.fragment !== undefined) {
      uriTokens.push("#", component.fragment);
    }
    return uriTokens.join("");
  }
  var URI_PARSE = /^(?:([^#/:?]+):)?(?:\/\/((?:([^#/?@]*)@)?(\[[^#/?\]]+\]|[^#/:?]*)(?::(\d*))?))?([^#?]*)(?:\?([^#]*))?(?:#((?:.|[\n\r])*))?/u;
  var AUTHORITY_PREFIX = /^(?:[^#/:?]+:)?\/\/([^/?#]*)/;
  var AUTHORITY_INTRODUCER_REGION = /^(?:[^#/:?]+:)?([/\\\t\n\r]*)/;
  function getParseError(parsed, matches) {
    if (matches[2] !== undefined && parsed.path && parsed.path[0] !== "/") {
      return 'URI path must start with "/" when authority is present.';
    }
    if (typeof parsed.port === "number" && (parsed.port < 0 || parsed.port > 65535)) {
      return "URI port is malformed.";
    }
    return;
  }
  function parseWithStatus(uri, opts) {
    const options = Object.assign({}, opts);
    const parsed = {
      scheme: undefined,
      userinfo: undefined,
      host: "",
      port: undefined,
      path: "",
      query: undefined,
      fragment: undefined
    };
    let malformedAuthorityOrPort = false;
    let isIP = false;
    if (options.reference === "suffix") {
      if (options.scheme) {
        uri = options.scheme + ":" + uri;
      } else {
        uri = "//" + uri;
      }
    }
    const authorityMatch = uri.match(AUTHORITY_PREFIX);
    if (authorityMatch !== null && authorityMatch[1].indexOf("\\") !== -1) {
      parsed.error = "URI authority must not contain a literal backslash.";
      malformedAuthorityOrPort = true;
    }
    const introducerMatch = uri.match(AUTHORITY_INTRODUCER_REGION);
    if (introducerMatch !== null) {
      const region = introducerMatch[1];
      const normalizedRegion = region.replace(/[\t\n\r]/g, "");
      if (normalizedRegion.length >= 2) {
        if (normalizedRegion.slice(0, 2) !== "//") {
          parsed.error = parsed.error || "URI authority must not contain a literal backslash.";
          malformedAuthorityOrPort = true;
        } else if (region.length !== normalizedRegion.length) {
          parsed.error = parsed.error || "URI authority introducer must not contain whitespace.";
          malformedAuthorityOrPort = true;
        }
      }
    }
    const matches = uri.match(URI_PARSE);
    if (matches) {
      parsed.scheme = matches[1];
      parsed.userinfo = matches[3];
      parsed.host = matches[4];
      parsed.port = parseInt(matches[5], 10);
      parsed.path = matches[6] || "";
      parsed.query = matches[7];
      parsed.fragment = matches[8];
      if (isNaN(parsed.port)) {
        parsed.port = matches[5];
      }
      const parseError = getParseError(parsed, matches);
      if (parseError !== undefined) {
        parsed.error = parsed.error || parseError;
        malformedAuthorityOrPort = true;
      }
      if (parsed.host) {
        const ipv4result = isIPv4(parsed.host);
        if (ipv4result === false) {
          const ipv6result = normalizeIPv6(parsed.host);
          parsed.host = ipv6result.host.toLowerCase();
          isIP = ipv6result.isIPV6;
        } else {
          isIP = true;
        }
      }
      if (parsed.scheme === undefined && parsed.userinfo === undefined && parsed.host === undefined && parsed.port === undefined && parsed.query === undefined && !parsed.path) {
        parsed.reference = "same-document";
      } else if (parsed.scheme === undefined) {
        parsed.reference = "relative";
      } else if (parsed.fragment === undefined) {
        parsed.reference = "absolute";
      } else {
        parsed.reference = "uri";
      }
      if (options.reference && options.reference !== "suffix" && options.reference !== parsed.reference) {
        parsed.error = parsed.error || "URI is not a " + options.reference + " reference.";
      }
      const schemeHandler = getSchemeHandler(options.scheme || parsed.scheme);
      if (!options.unicodeSupport && (!schemeHandler || !schemeHandler.unicodeSupport)) {
        if (parsed.host && (options.domainHost || schemeHandler && schemeHandler.domainHost) && isIP === false && nonSimpleDomain(parsed.host)) {
          try {
            parsed.host = new URL("http://" + parsed.host).hostname;
          } catch (e) {
            parsed.error = parsed.error || "Host's domain name can not be converted to ASCII: " + e;
          }
        }
      }
      if (!schemeHandler || schemeHandler && !schemeHandler.skipNormalize) {
        if (uri.indexOf("%") !== -1) {
          if (parsed.scheme !== undefined) {
            parsed.scheme = unescape(parsed.scheme);
          }
          if (parsed.host !== undefined) {
            parsed.host = reescapeHostDelimiters(unescape(parsed.host), isIP);
          }
        }
        if (parsed.path) {
          parsed.path = normalizePathEncoding(parsed.path);
        }
        if (parsed.fragment) {
          try {
            parsed.fragment = encodeURI(decodeURIComponent(parsed.fragment));
          } catch {
            parsed.error = parsed.error || "URI malformed";
          }
        }
      }
      if (schemeHandler && schemeHandler.parse) {
        schemeHandler.parse(parsed, options);
      }
    } else {
      parsed.error = parsed.error || "URI can not be parsed.";
    }
    return { parsed, malformedAuthorityOrPort };
  }
  function parse(uri, opts) {
    return parseWithStatus(uri, opts).parsed;
  }
  function normalizeString(uri, opts) {
    return normalizeStringWithStatus(uri, opts).normalized;
  }
  function normalizeStringWithStatus(uri, opts) {
    const { parsed, malformedAuthorityOrPort } = parseWithStatus(uri, opts);
    return {
      normalized: malformedAuthorityOrPort ? uri : serialize(parsed, opts),
      malformedAuthorityOrPort
    };
  }
  function normalizeComparableURI(uri, opts) {
    if (typeof uri === "string") {
      const { normalized, malformedAuthorityOrPort } = normalizeStringWithStatus(uri, opts);
      return malformedAuthorityOrPort ? undefined : normalized;
    }
    if (typeof uri === "object") {
      return serialize(uri, opts);
    }
  }
  var fastUri = {
    SCHEMES,
    normalize,
    resolve,
    resolveComponent,
    equal,
    serialize,
    parse
  };
  module.exports = fastUri;
  module.exports.default = fastUri;
  module.exports.fastUri = fastUri;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/runtime/uri.js
var require_uri = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var uri = require_fast_uri();
  uri.code = 'require("ajv/dist/runtime/uri").default';
  exports.default = uri;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/core.js
var require_core = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.CodeGen = exports.Name = exports.nil = exports.stringify = exports.str = exports._ = exports.KeywordCxt = undefined;
  var validate_1 = require_validate();
  Object.defineProperty(exports, "KeywordCxt", { enumerable: true, get: function() {
    return validate_1.KeywordCxt;
  } });
  var codegen_1 = require_codegen();
  Object.defineProperty(exports, "_", { enumerable: true, get: function() {
    return codegen_1._;
  } });
  Object.defineProperty(exports, "str", { enumerable: true, get: function() {
    return codegen_1.str;
  } });
  Object.defineProperty(exports, "stringify", { enumerable: true, get: function() {
    return codegen_1.stringify;
  } });
  Object.defineProperty(exports, "nil", { enumerable: true, get: function() {
    return codegen_1.nil;
  } });
  Object.defineProperty(exports, "Name", { enumerable: true, get: function() {
    return codegen_1.Name;
  } });
  Object.defineProperty(exports, "CodeGen", { enumerable: true, get: function() {
    return codegen_1.CodeGen;
  } });
  var validation_error_1 = require_validation_error();
  var ref_error_1 = require_ref_error();
  var rules_1 = require_rules();
  var compile_1 = require_compile();
  var codegen_2 = require_codegen();
  var resolve_1 = require_resolve();
  var dataType_1 = require_dataType();
  var util_1 = require_util();
  var $dataRefSchema = require_data();
  var uri_1 = require_uri();
  var defaultRegExp = (str, flags) => new RegExp(str, flags);
  defaultRegExp.code = "new RegExp";
  var META_IGNORE_OPTIONS = ["removeAdditional", "useDefaults", "coerceTypes"];
  var EXT_SCOPE_NAMES = new Set([
    "validate",
    "serialize",
    "parse",
    "wrapper",
    "root",
    "schema",
    "keyword",
    "pattern",
    "formats",
    "validate$data",
    "func",
    "obj",
    "Error"
  ]);
  var removedOptions = {
    errorDataPath: "",
    format: "`validateFormats: false` can be used instead.",
    nullable: '"nullable" keyword is supported by default.',
    jsonPointers: "Deprecated jsPropertySyntax can be used instead.",
    extendRefs: "Deprecated ignoreKeywordsWithRef can be used instead.",
    missingRefs: "Pass empty schema with $id that should be ignored to ajv.addSchema.",
    processCode: "Use option `code: {process: (code, schemaEnv: object) => string}`",
    sourceCode: "Use option `code: {source: true}`",
    strictDefaults: "It is default now, see option `strict`.",
    strictKeywords: "It is default now, see option `strict`.",
    uniqueItems: '"uniqueItems" keyword is always validated.',
    unknownFormats: "Disable strict mode or pass `true` to `ajv.addFormat` (or `formats` option).",
    cache: "Map is used as cache, schema object as key.",
    serialize: "Map is used as cache, schema object as key.",
    ajvErrors: "It is default now."
  };
  var deprecatedOptions = {
    ignoreKeywordsWithRef: "",
    jsPropertySyntax: "",
    unicode: '"minLength"/"maxLength" account for unicode characters by default.'
  };
  var MAX_EXPRESSION = 200;
  function requiredOptions(o) {
    var _a, _b, _c, _d, _e, _f, _g, _h, _j, _k, _l, _m, _o, _p, _q, _r, _s, _t, _u, _v, _w, _x, _y, _z, _0;
    const s = o.strict;
    const _optz = (_a = o.code) === null || _a === undefined ? undefined : _a.optimize;
    const optimize = _optz === true || _optz === undefined ? 1 : _optz || 0;
    const regExp = (_c = (_b = o.code) === null || _b === undefined ? undefined : _b.regExp) !== null && _c !== undefined ? _c : defaultRegExp;
    const uriResolver = (_d = o.uriResolver) !== null && _d !== undefined ? _d : uri_1.default;
    return {
      strictSchema: (_f = (_e = o.strictSchema) !== null && _e !== undefined ? _e : s) !== null && _f !== undefined ? _f : true,
      strictNumbers: (_h = (_g = o.strictNumbers) !== null && _g !== undefined ? _g : s) !== null && _h !== undefined ? _h : true,
      strictTypes: (_k = (_j = o.strictTypes) !== null && _j !== undefined ? _j : s) !== null && _k !== undefined ? _k : "log",
      strictTuples: (_m = (_l = o.strictTuples) !== null && _l !== undefined ? _l : s) !== null && _m !== undefined ? _m : "log",
      strictRequired: (_p = (_o = o.strictRequired) !== null && _o !== undefined ? _o : s) !== null && _p !== undefined ? _p : false,
      code: o.code ? { ...o.code, optimize, regExp } : { optimize, regExp },
      loopRequired: (_q = o.loopRequired) !== null && _q !== undefined ? _q : MAX_EXPRESSION,
      loopEnum: (_r = o.loopEnum) !== null && _r !== undefined ? _r : MAX_EXPRESSION,
      meta: (_s = o.meta) !== null && _s !== undefined ? _s : true,
      messages: (_t = o.messages) !== null && _t !== undefined ? _t : true,
      inlineRefs: (_u = o.inlineRefs) !== null && _u !== undefined ? _u : true,
      schemaId: (_v = o.schemaId) !== null && _v !== undefined ? _v : "$id",
      addUsedSchema: (_w = o.addUsedSchema) !== null && _w !== undefined ? _w : true,
      validateSchema: (_x = o.validateSchema) !== null && _x !== undefined ? _x : true,
      validateFormats: (_y = o.validateFormats) !== null && _y !== undefined ? _y : true,
      unicodeRegExp: (_z = o.unicodeRegExp) !== null && _z !== undefined ? _z : true,
      int32range: (_0 = o.int32range) !== null && _0 !== undefined ? _0 : true,
      uriResolver
    };
  }

  class Ajv {
    constructor(opts = {}) {
      this.schemas = {};
      this.refs = {};
      this.formats = {};
      this._compilations = new Set;
      this._loading = {};
      this._cache = new Map;
      opts = this.opts = { ...opts, ...requiredOptions(opts) };
      const { es5, lines } = this.opts.code;
      this.scope = new codegen_2.ValueScope({ scope: {}, prefixes: EXT_SCOPE_NAMES, es5, lines });
      this.logger = getLogger(opts.logger);
      const formatOpt = opts.validateFormats;
      opts.validateFormats = false;
      this.RULES = (0, rules_1.getRules)();
      checkOptions.call(this, removedOptions, opts, "NOT SUPPORTED");
      checkOptions.call(this, deprecatedOptions, opts, "DEPRECATED", "warn");
      this._metaOpts = getMetaSchemaOptions.call(this);
      if (opts.formats)
        addInitialFormats.call(this);
      this._addVocabularies();
      this._addDefaultMetaSchema();
      if (opts.keywords)
        addInitialKeywords.call(this, opts.keywords);
      if (typeof opts.meta == "object")
        this.addMetaSchema(opts.meta);
      addInitialSchemas.call(this);
      opts.validateFormats = formatOpt;
    }
    _addVocabularies() {
      this.addKeyword("$async");
    }
    _addDefaultMetaSchema() {
      const { $data, meta, schemaId } = this.opts;
      let _dataRefSchema = $dataRefSchema;
      if (schemaId === "id") {
        _dataRefSchema = { ...$dataRefSchema };
        _dataRefSchema.id = _dataRefSchema.$id;
        delete _dataRefSchema.$id;
      }
      if (meta && $data)
        this.addMetaSchema(_dataRefSchema, _dataRefSchema[schemaId], false);
    }
    defaultMeta() {
      const { meta, schemaId } = this.opts;
      return this.opts.defaultMeta = typeof meta == "object" ? meta[schemaId] || meta : undefined;
    }
    validate(schemaKeyRef, data) {
      let v;
      if (typeof schemaKeyRef == "string") {
        v = this.getSchema(schemaKeyRef);
        if (!v)
          throw new Error(`no schema with key or ref "${schemaKeyRef}"`);
      } else {
        v = this.compile(schemaKeyRef);
      }
      const valid = v(data);
      if (!("$async" in v))
        this.errors = v.errors;
      return valid;
    }
    compile(schema, _meta) {
      const sch = this._addSchema(schema, _meta);
      return sch.validate || this._compileSchemaEnv(sch);
    }
    compileAsync(schema, meta) {
      if (typeof this.opts.loadSchema != "function") {
        throw new Error("options.loadSchema should be a function");
      }
      const { loadSchema } = this.opts;
      return runCompileAsync.call(this, schema, meta);
      async function runCompileAsync(_schema, _meta) {
        await loadMetaSchema.call(this, _schema.$schema);
        const sch = this._addSchema(_schema, _meta);
        return sch.validate || _compileAsync.call(this, sch);
      }
      async function loadMetaSchema($ref) {
        if ($ref && !this.getSchema($ref)) {
          await runCompileAsync.call(this, { $ref }, true);
        }
      }
      async function _compileAsync(sch) {
        try {
          return this._compileSchemaEnv(sch);
        } catch (e) {
          if (!(e instanceof ref_error_1.default))
            throw e;
          checkLoaded.call(this, e);
          await loadMissingSchema.call(this, e.missingSchema);
          return _compileAsync.call(this, sch);
        }
      }
      function checkLoaded({ missingSchema: ref, missingRef }) {
        if (this.refs[ref]) {
          throw new Error(`AnySchema ${ref} is loaded but ${missingRef} cannot be resolved`);
        }
      }
      async function loadMissingSchema(ref) {
        const _schema = await _loadSchema.call(this, ref);
        if (!this.refs[ref])
          await loadMetaSchema.call(this, _schema.$schema);
        if (!this.refs[ref])
          this.addSchema(_schema, ref, meta);
      }
      async function _loadSchema(ref) {
        const p = this._loading[ref];
        if (p)
          return p;
        try {
          return await (this._loading[ref] = loadSchema(ref));
        } finally {
          delete this._loading[ref];
        }
      }
    }
    addSchema(schema, key, _meta, _validateSchema = this.opts.validateSchema) {
      if (Array.isArray(schema)) {
        for (const sch of schema)
          this.addSchema(sch, undefined, _meta, _validateSchema);
        return this;
      }
      let id;
      if (typeof schema === "object") {
        const { schemaId } = this.opts;
        id = schema[schemaId];
        if (id !== undefined && typeof id != "string") {
          throw new Error(`schema ${schemaId} must be string`);
        }
      }
      key = (0, resolve_1.normalizeId)(key || id);
      this._checkUnique(key);
      this.schemas[key] = this._addSchema(schema, _meta, key, _validateSchema, true);
      return this;
    }
    addMetaSchema(schema, key, _validateSchema = this.opts.validateSchema) {
      this.addSchema(schema, key, true, _validateSchema);
      return this;
    }
    validateSchema(schema, throwOrLogError) {
      if (typeof schema == "boolean")
        return true;
      let $schema;
      $schema = schema.$schema;
      if ($schema !== undefined && typeof $schema != "string") {
        throw new Error("$schema must be a string");
      }
      $schema = $schema || this.opts.defaultMeta || this.defaultMeta();
      if (!$schema) {
        this.logger.warn("meta-schema not available");
        this.errors = null;
        return true;
      }
      const valid = this.validate($schema, schema);
      if (!valid && throwOrLogError) {
        const message = "schema is invalid: " + this.errorsText();
        if (this.opts.validateSchema === "log")
          this.logger.error(message);
        else
          throw new Error(message);
      }
      return valid;
    }
    getSchema(keyRef) {
      let sch;
      while (typeof (sch = getSchEnv.call(this, keyRef)) == "string")
        keyRef = sch;
      if (sch === undefined) {
        const { schemaId } = this.opts;
        const root = new compile_1.SchemaEnv({ schema: {}, schemaId });
        sch = compile_1.resolveSchema.call(this, root, keyRef);
        if (!sch)
          return;
        this.refs[keyRef] = sch;
      }
      return sch.validate || this._compileSchemaEnv(sch);
    }
    removeSchema(schemaKeyRef) {
      if (schemaKeyRef instanceof RegExp) {
        this._removeAllSchemas(this.schemas, schemaKeyRef);
        this._removeAllSchemas(this.refs, schemaKeyRef);
        return this;
      }
      switch (typeof schemaKeyRef) {
        case "undefined":
          this._removeAllSchemas(this.schemas);
          this._removeAllSchemas(this.refs);
          this._cache.clear();
          return this;
        case "string": {
          const sch = getSchEnv.call(this, schemaKeyRef);
          if (typeof sch == "object")
            this._cache.delete(sch.schema);
          delete this.schemas[schemaKeyRef];
          delete this.refs[schemaKeyRef];
          return this;
        }
        case "object": {
          const cacheKey = schemaKeyRef;
          this._cache.delete(cacheKey);
          let id = schemaKeyRef[this.opts.schemaId];
          if (id) {
            id = (0, resolve_1.normalizeId)(id);
            delete this.schemas[id];
            delete this.refs[id];
          }
          return this;
        }
        default:
          throw new Error("ajv.removeSchema: invalid parameter");
      }
    }
    addVocabulary(definitions) {
      for (const def of definitions)
        this.addKeyword(def);
      return this;
    }
    addKeyword(kwdOrDef, def) {
      let keyword;
      if (typeof kwdOrDef == "string") {
        keyword = kwdOrDef;
        if (typeof def == "object") {
          this.logger.warn("these parameters are deprecated, see docs for addKeyword");
          def.keyword = keyword;
        }
      } else if (typeof kwdOrDef == "object" && def === undefined) {
        def = kwdOrDef;
        keyword = def.keyword;
        if (Array.isArray(keyword) && !keyword.length) {
          throw new Error("addKeywords: keyword must be string or non-empty array");
        }
      } else {
        throw new Error("invalid addKeywords parameters");
      }
      checkKeyword.call(this, keyword, def);
      if (!def) {
        (0, util_1.eachItem)(keyword, (kwd) => addRule.call(this, kwd));
        return this;
      }
      keywordMetaschema.call(this, def);
      const definition = {
        ...def,
        type: (0, dataType_1.getJSONTypes)(def.type),
        schemaType: (0, dataType_1.getJSONTypes)(def.schemaType)
      };
      (0, util_1.eachItem)(keyword, definition.type.length === 0 ? (k) => addRule.call(this, k, definition) : (k) => definition.type.forEach((t) => addRule.call(this, k, definition, t)));
      return this;
    }
    getKeyword(keyword) {
      const rule = this.RULES.all[keyword];
      return typeof rule == "object" ? rule.definition : !!rule;
    }
    removeKeyword(keyword) {
      const { RULES } = this;
      delete RULES.keywords[keyword];
      delete RULES.all[keyword];
      for (const group of RULES.rules) {
        const i = group.rules.findIndex((rule) => rule.keyword === keyword);
        if (i >= 0)
          group.rules.splice(i, 1);
      }
      return this;
    }
    addFormat(name, format) {
      if (typeof format == "string")
        format = new RegExp(format);
      this.formats[name] = format;
      return this;
    }
    errorsText(errors = this.errors, { separator = ", ", dataVar = "data" } = {}) {
      if (!errors || errors.length === 0)
        return "No errors";
      return errors.map((e) => `${dataVar}${e.instancePath} ${e.message}`).reduce((text, msg) => text + separator + msg);
    }
    $dataMetaSchema(metaSchema, keywordsJsonPointers) {
      const rules = this.RULES.all;
      metaSchema = JSON.parse(JSON.stringify(metaSchema));
      for (const jsonPointer of keywordsJsonPointers) {
        const segments = jsonPointer.split("/").slice(1);
        let keywords = metaSchema;
        for (const seg of segments)
          keywords = keywords[seg];
        for (const key in rules) {
          const rule = rules[key];
          if (typeof rule != "object")
            continue;
          const { $data } = rule.definition;
          const schema = keywords[key];
          if ($data && schema)
            keywords[key] = schemaOrData(schema);
        }
      }
      return metaSchema;
    }
    _removeAllSchemas(schemas, regex) {
      for (const keyRef in schemas) {
        const sch = schemas[keyRef];
        if (!regex || regex.test(keyRef)) {
          if (typeof sch == "string") {
            delete schemas[keyRef];
          } else if (sch && !sch.meta) {
            this._cache.delete(sch.schema);
            delete schemas[keyRef];
          }
        }
      }
    }
    _addSchema(schema, meta, baseId, validateSchema = this.opts.validateSchema, addSchema = this.opts.addUsedSchema) {
      let id;
      const { schemaId } = this.opts;
      if (typeof schema == "object") {
        id = schema[schemaId];
      } else {
        if (this.opts.jtd)
          throw new Error("schema must be object");
        else if (typeof schema != "boolean")
          throw new Error("schema must be object or boolean");
      }
      let sch = this._cache.get(schema);
      if (sch !== undefined)
        return sch;
      baseId = (0, resolve_1.normalizeId)(id || baseId);
      const localRefs = resolve_1.getSchemaRefs.call(this, schema, baseId);
      sch = new compile_1.SchemaEnv({ schema, schemaId, meta, baseId, localRefs });
      this._cache.set(sch.schema, sch);
      if (addSchema && !baseId.startsWith("#")) {
        if (baseId)
          this._checkUnique(baseId);
        this.refs[baseId] = sch;
      }
      if (validateSchema)
        this.validateSchema(schema, true);
      return sch;
    }
    _checkUnique(id) {
      if (this.schemas[id] || this.refs[id]) {
        throw new Error(`schema with key or id "${id}" already exists`);
      }
    }
    _compileSchemaEnv(sch) {
      if (sch.meta)
        this._compileMetaSchema(sch);
      else
        compile_1.compileSchema.call(this, sch);
      if (!sch.validate)
        throw new Error("ajv implementation error");
      return sch.validate;
    }
    _compileMetaSchema(sch) {
      const currentOpts = this.opts;
      this.opts = this._metaOpts;
      try {
        compile_1.compileSchema.call(this, sch);
      } finally {
        this.opts = currentOpts;
      }
    }
  }
  Ajv.ValidationError = validation_error_1.default;
  Ajv.MissingRefError = ref_error_1.default;
  exports.default = Ajv;
  function checkOptions(checkOpts, options, msg, log = "error") {
    for (const key in checkOpts) {
      const opt = key;
      if (opt in options)
        this.logger[log](`${msg}: option ${key}. ${checkOpts[opt]}`);
    }
  }
  function getSchEnv(keyRef) {
    keyRef = (0, resolve_1.normalizeId)(keyRef);
    return this.schemas[keyRef] || this.refs[keyRef];
  }
  function addInitialSchemas() {
    const optsSchemas = this.opts.schemas;
    if (!optsSchemas)
      return;
    if (Array.isArray(optsSchemas))
      this.addSchema(optsSchemas);
    else
      for (const key in optsSchemas)
        this.addSchema(optsSchemas[key], key);
  }
  function addInitialFormats() {
    for (const name in this.opts.formats) {
      const format = this.opts.formats[name];
      if (format)
        this.addFormat(name, format);
    }
  }
  function addInitialKeywords(defs) {
    if (Array.isArray(defs)) {
      this.addVocabulary(defs);
      return;
    }
    this.logger.warn("keywords option as map is deprecated, pass array");
    for (const keyword in defs) {
      const def = defs[keyword];
      if (!def.keyword)
        def.keyword = keyword;
      this.addKeyword(def);
    }
  }
  function getMetaSchemaOptions() {
    const metaOpts = { ...this.opts };
    for (const opt of META_IGNORE_OPTIONS)
      delete metaOpts[opt];
    return metaOpts;
  }
  var noLogs = { log() {}, warn() {}, error() {} };
  function getLogger(logger) {
    if (logger === false)
      return noLogs;
    if (logger === undefined)
      return console;
    if (logger.log && logger.warn && logger.error)
      return logger;
    throw new Error("logger must implement log, warn and error methods");
  }
  var KEYWORD_NAME = /^[a-z_$][a-z0-9_$:-]*$/i;
  function checkKeyword(keyword, def) {
    const { RULES } = this;
    (0, util_1.eachItem)(keyword, (kwd) => {
      if (RULES.keywords[kwd])
        throw new Error(`Keyword ${kwd} is already defined`);
      if (!KEYWORD_NAME.test(kwd))
        throw new Error(`Keyword ${kwd} has invalid name`);
    });
    if (!def)
      return;
    if (def.$data && !(("code" in def) || ("validate" in def))) {
      throw new Error('$data keyword must have "code" or "validate" function');
    }
  }
  function addRule(keyword, definition, dataType) {
    var _a;
    const post = definition === null || definition === undefined ? undefined : definition.post;
    if (dataType && post)
      throw new Error('keyword with "post" flag cannot have "type"');
    const { RULES } = this;
    let ruleGroup = post ? RULES.post : RULES.rules.find(({ type: t }) => t === dataType);
    if (!ruleGroup) {
      ruleGroup = { type: dataType, rules: [] };
      RULES.rules.push(ruleGroup);
    }
    RULES.keywords[keyword] = true;
    if (!definition)
      return;
    const rule = {
      keyword,
      definition: {
        ...definition,
        type: (0, dataType_1.getJSONTypes)(definition.type),
        schemaType: (0, dataType_1.getJSONTypes)(definition.schemaType)
      }
    };
    if (definition.before)
      addBeforeRule.call(this, ruleGroup, rule, definition.before);
    else
      ruleGroup.rules.push(rule);
    RULES.all[keyword] = rule;
    (_a = definition.implements) === null || _a === undefined || _a.forEach((kwd) => this.addKeyword(kwd));
  }
  function addBeforeRule(ruleGroup, rule, before) {
    const i = ruleGroup.rules.findIndex((_rule) => _rule.keyword === before);
    if (i >= 0) {
      ruleGroup.rules.splice(i, 0, rule);
    } else {
      ruleGroup.rules.push(rule);
      this.logger.warn(`rule ${before} is not defined`);
    }
  }
  function keywordMetaschema(def) {
    let { metaSchema } = def;
    if (metaSchema === undefined)
      return;
    if (def.$data && this.opts.$data)
      metaSchema = schemaOrData(metaSchema);
    def.validateSchema = this.compile(metaSchema, true);
  }
  var $dataRef = {
    $ref: "https://raw.githubusercontent.com/ajv-validator/ajv/master/lib/refs/data.json#"
  };
  function schemaOrData(schema) {
    return { anyOf: [schema, $dataRef] };
  }
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/core/id.js
var require_id = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var def = {
    keyword: "id",
    code() {
      throw new Error('NOT SUPPORTED: keyword "id", use "$id" for schema ID');
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/core/ref.js
var require_ref = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.callRef = exports.getValidate = undefined;
  var ref_error_1 = require_ref_error();
  var code_1 = require_code2();
  var codegen_1 = require_codegen();
  var names_1 = require_names();
  var compile_1 = require_compile();
  var util_1 = require_util();
  var def = {
    keyword: "$ref",
    schemaType: "string",
    code(cxt) {
      const { gen, schema: $ref, it } = cxt;
      const { baseId, schemaEnv: env, validateName, opts, self } = it;
      const { root } = env;
      if (($ref === "#" || $ref === "#/") && baseId === root.baseId)
        return callRootRef();
      const schOrEnv = compile_1.resolveRef.call(self, root, baseId, $ref);
      if (schOrEnv === undefined)
        throw new ref_error_1.default(it.opts.uriResolver, baseId, $ref);
      if (schOrEnv instanceof compile_1.SchemaEnv)
        return callValidate(schOrEnv);
      return inlineRefSchema(schOrEnv);
      function callRootRef() {
        if (env === root)
          return callRef(cxt, validateName, env, env.$async);
        const rootName = gen.scopeValue("root", { ref: root });
        return callRef(cxt, (0, codegen_1._)`${rootName}.validate`, root, root.$async);
      }
      function callValidate(sch) {
        const v = getValidate(cxt, sch);
        callRef(cxt, v, sch, sch.$async);
      }
      function inlineRefSchema(sch) {
        const schName = gen.scopeValue("schema", opts.code.source === true ? { ref: sch, code: (0, codegen_1.stringify)(sch) } : { ref: sch });
        const valid = gen.name("valid");
        const schCxt = cxt.subschema({
          schema: sch,
          dataTypes: [],
          schemaPath: codegen_1.nil,
          topSchemaRef: schName,
          errSchemaPath: $ref
        }, valid);
        cxt.mergeEvaluated(schCxt);
        cxt.ok(valid);
      }
    }
  };
  function getValidate(cxt, sch) {
    const { gen } = cxt;
    return sch.validate ? gen.scopeValue("validate", { ref: sch.validate }) : (0, codegen_1._)`${gen.scopeValue("wrapper", { ref: sch })}.validate`;
  }
  exports.getValidate = getValidate;
  function callRef(cxt, v, sch, $async) {
    const { gen, it } = cxt;
    const { allErrors, schemaEnv: env, opts } = it;
    const passCxt = opts.passContext ? names_1.default.this : codegen_1.nil;
    if ($async)
      callAsyncRef();
    else
      callSyncRef();
    function callAsyncRef() {
      if (!env.$async)
        throw new Error("async schema referenced by sync schema");
      const valid = gen.let("valid");
      gen.try(() => {
        gen.code((0, codegen_1._)`await ${(0, code_1.callValidateCode)(cxt, v, passCxt)}`);
        addEvaluatedFrom(v);
        if (!allErrors)
          gen.assign(valid, true);
      }, (e) => {
        gen.if((0, codegen_1._)`!(${e} instanceof ${it.ValidationError})`, () => gen.throw(e));
        addErrorsFrom(e);
        if (!allErrors)
          gen.assign(valid, false);
      });
      cxt.ok(valid);
    }
    function callSyncRef() {
      cxt.result((0, code_1.callValidateCode)(cxt, v, passCxt), () => addEvaluatedFrom(v), () => addErrorsFrom(v));
    }
    function addErrorsFrom(source) {
      const errs = (0, codegen_1._)`${source}.errors`;
      gen.assign(names_1.default.vErrors, (0, codegen_1._)`${names_1.default.vErrors} === null ? ${errs} : ${names_1.default.vErrors}.concat(${errs})`);
      gen.assign(names_1.default.errors, (0, codegen_1._)`${names_1.default.vErrors}.length`);
    }
    function addEvaluatedFrom(source) {
      var _a;
      if (!it.opts.unevaluated)
        return;
      const schEvaluated = (_a = sch === null || sch === undefined ? undefined : sch.validate) === null || _a === undefined ? undefined : _a.evaluated;
      if (it.props !== true) {
        if (schEvaluated && !schEvaluated.dynamicProps) {
          if (schEvaluated.props !== undefined) {
            it.props = util_1.mergeEvaluated.props(gen, schEvaluated.props, it.props);
          }
        } else {
          const props = gen.var("props", (0, codegen_1._)`${source}.evaluated.props`);
          it.props = util_1.mergeEvaluated.props(gen, props, it.props, codegen_1.Name);
        }
      }
      if (it.items !== true) {
        if (schEvaluated && !schEvaluated.dynamicItems) {
          if (schEvaluated.items !== undefined) {
            it.items = util_1.mergeEvaluated.items(gen, schEvaluated.items, it.items);
          }
        } else {
          const items = gen.var("items", (0, codegen_1._)`${source}.evaluated.items`);
          it.items = util_1.mergeEvaluated.items(gen, items, it.items, codegen_1.Name);
        }
      }
    }
  }
  exports.callRef = callRef;
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/core/index.js
var require_core2 = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var id_1 = require_id();
  var ref_1 = require_ref();
  var core = [
    "$schema",
    "$id",
    "$defs",
    "$vocabulary",
    { keyword: "$comment" },
    "definitions",
    id_1.default,
    ref_1.default
  ];
  exports.default = core;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/validation/limitNumber.js
var require_limitNumber = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var codegen_1 = require_codegen();
  var ops = codegen_1.operators;
  var KWDs = {
    maximum: { okStr: "<=", ok: ops.LTE, fail: ops.GT },
    minimum: { okStr: ">=", ok: ops.GTE, fail: ops.LT },
    exclusiveMaximum: { okStr: "<", ok: ops.LT, fail: ops.GTE },
    exclusiveMinimum: { okStr: ">", ok: ops.GT, fail: ops.LTE }
  };
  var error = {
    message: ({ keyword, schemaCode }) => (0, codegen_1.str)`must be ${KWDs[keyword].okStr} ${schemaCode}`,
    params: ({ keyword, schemaCode }) => (0, codegen_1._)`{comparison: ${KWDs[keyword].okStr}, limit: ${schemaCode}}`
  };
  var def = {
    keyword: Object.keys(KWDs),
    type: "number",
    schemaType: "number",
    $data: true,
    error,
    code(cxt) {
      const { keyword, data, schemaCode } = cxt;
      cxt.fail$data((0, codegen_1._)`${data} ${KWDs[keyword].fail} ${schemaCode} || isNaN(${data})`);
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/validation/multipleOf.js
var require_multipleOf = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var codegen_1 = require_codegen();
  var error = {
    message: ({ schemaCode }) => (0, codegen_1.str)`must be multiple of ${schemaCode}`,
    params: ({ schemaCode }) => (0, codegen_1._)`{multipleOf: ${schemaCode}}`
  };
  var def = {
    keyword: "multipleOf",
    type: "number",
    schemaType: "number",
    $data: true,
    error,
    code(cxt) {
      const { gen, data, schemaCode, it } = cxt;
      const prec = it.opts.multipleOfPrecision;
      const res = gen.let("res");
      const invalid = prec ? (0, codegen_1._)`Math.abs(Math.round(${res}) - ${res}) > 1e-${prec}` : (0, codegen_1._)`${res} !== parseInt(${res})`;
      cxt.fail$data((0, codegen_1._)`(${schemaCode} === 0 || (${res} = ${data}/${schemaCode}, ${invalid}))`);
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/runtime/ucs2length.js
var require_ucs2length = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  function ucs2length(str) {
    const len = str.length;
    let length = 0;
    let pos = 0;
    let value;
    while (pos < len) {
      length++;
      value = str.charCodeAt(pos++);
      if (value >= 55296 && value <= 56319 && pos < len) {
        value = str.charCodeAt(pos);
        if ((value & 64512) === 56320)
          pos++;
      }
    }
    return length;
  }
  exports.default = ucs2length;
  ucs2length.code = 'require("ajv/dist/runtime/ucs2length").default';
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/validation/limitLength.js
var require_limitLength = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var codegen_1 = require_codegen();
  var util_1 = require_util();
  var ucs2length_1 = require_ucs2length();
  var error = {
    message({ keyword, schemaCode }) {
      const comp = keyword === "maxLength" ? "more" : "fewer";
      return (0, codegen_1.str)`must NOT have ${comp} than ${schemaCode} characters`;
    },
    params: ({ schemaCode }) => (0, codegen_1._)`{limit: ${schemaCode}}`
  };
  var def = {
    keyword: ["maxLength", "minLength"],
    type: "string",
    schemaType: "number",
    $data: true,
    error,
    code(cxt) {
      const { keyword, data, schemaCode, it } = cxt;
      const op = keyword === "maxLength" ? codegen_1.operators.GT : codegen_1.operators.LT;
      const len = it.opts.unicode === false ? (0, codegen_1._)`${data}.length` : (0, codegen_1._)`${(0, util_1.useFunc)(cxt.gen, ucs2length_1.default)}(${data})`;
      cxt.fail$data((0, codegen_1._)`${len} ${op} ${schemaCode}`);
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/validation/pattern.js
var require_pattern = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var code_1 = require_code2();
  var codegen_1 = require_codegen();
  var error = {
    message: ({ schemaCode }) => (0, codegen_1.str)`must match pattern "${schemaCode}"`,
    params: ({ schemaCode }) => (0, codegen_1._)`{pattern: ${schemaCode}}`
  };
  var def = {
    keyword: "pattern",
    type: "string",
    schemaType: "string",
    $data: true,
    error,
    code(cxt) {
      const { data, $data, schema, schemaCode, it } = cxt;
      const u = it.opts.unicodeRegExp ? "u" : "";
      const regExp = $data ? (0, codegen_1._)`(new RegExp(${schemaCode}, ${u}))` : (0, code_1.usePattern)(cxt, schema);
      cxt.fail$data((0, codegen_1._)`!${regExp}.test(${data})`);
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/validation/limitProperties.js
var require_limitProperties = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var codegen_1 = require_codegen();
  var error = {
    message({ keyword, schemaCode }) {
      const comp = keyword === "maxProperties" ? "more" : "fewer";
      return (0, codegen_1.str)`must NOT have ${comp} than ${schemaCode} properties`;
    },
    params: ({ schemaCode }) => (0, codegen_1._)`{limit: ${schemaCode}}`
  };
  var def = {
    keyword: ["maxProperties", "minProperties"],
    type: "object",
    schemaType: "number",
    $data: true,
    error,
    code(cxt) {
      const { keyword, data, schemaCode } = cxt;
      const op = keyword === "maxProperties" ? codegen_1.operators.GT : codegen_1.operators.LT;
      cxt.fail$data((0, codegen_1._)`Object.keys(${data}).length ${op} ${schemaCode}`);
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/validation/required.js
var require_required = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var code_1 = require_code2();
  var codegen_1 = require_codegen();
  var util_1 = require_util();
  var error = {
    message: ({ params: { missingProperty } }) => (0, codegen_1.str)`must have required property '${missingProperty}'`,
    params: ({ params: { missingProperty } }) => (0, codegen_1._)`{missingProperty: ${missingProperty}}`
  };
  var def = {
    keyword: "required",
    type: "object",
    schemaType: "array",
    $data: true,
    error,
    code(cxt) {
      const { gen, schema, schemaCode, data, $data, it } = cxt;
      const { opts } = it;
      if (!$data && schema.length === 0)
        return;
      const useLoop = schema.length >= opts.loopRequired;
      if (it.allErrors)
        allErrorsMode();
      else
        exitOnErrorMode();
      if (opts.strictRequired) {
        const props = cxt.parentSchema.properties;
        const { definedProperties } = cxt.it;
        for (const requiredKey of schema) {
          if ((props === null || props === undefined ? undefined : props[requiredKey]) === undefined && !definedProperties.has(requiredKey)) {
            const schemaPath = it.schemaEnv.baseId + it.errSchemaPath;
            const msg = `required property "${requiredKey}" is not defined at "${schemaPath}" (strictRequired)`;
            (0, util_1.checkStrictMode)(it, msg, it.opts.strictRequired);
          }
        }
      }
      function allErrorsMode() {
        if (useLoop || $data) {
          cxt.block$data(codegen_1.nil, loopAllRequired);
        } else {
          for (const prop of schema) {
            (0, code_1.checkReportMissingProp)(cxt, prop);
          }
        }
      }
      function exitOnErrorMode() {
        const missing = gen.let("missing");
        if (useLoop || $data) {
          const valid = gen.let("valid", true);
          cxt.block$data(valid, () => loopUntilMissing(missing, valid));
          cxt.ok(valid);
        } else {
          gen.if((0, code_1.checkMissingProp)(cxt, schema, missing));
          (0, code_1.reportMissingProp)(cxt, missing);
          gen.else();
        }
      }
      function loopAllRequired() {
        gen.forOf("prop", schemaCode, (prop) => {
          cxt.setParams({ missingProperty: prop });
          gen.if((0, code_1.noPropertyInData)(gen, data, prop, opts.ownProperties), () => cxt.error());
        });
      }
      function loopUntilMissing(missing, valid) {
        cxt.setParams({ missingProperty: missing });
        gen.forOf(missing, schemaCode, () => {
          gen.assign(valid, (0, code_1.propertyInData)(gen, data, missing, opts.ownProperties));
          gen.if((0, codegen_1.not)(valid), () => {
            cxt.error();
            gen.break();
          });
        }, codegen_1.nil);
      }
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/validation/limitItems.js
var require_limitItems = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var codegen_1 = require_codegen();
  var error = {
    message({ keyword, schemaCode }) {
      const comp = keyword === "maxItems" ? "more" : "fewer";
      return (0, codegen_1.str)`must NOT have ${comp} than ${schemaCode} items`;
    },
    params: ({ schemaCode }) => (0, codegen_1._)`{limit: ${schemaCode}}`
  };
  var def = {
    keyword: ["maxItems", "minItems"],
    type: "array",
    schemaType: "number",
    $data: true,
    error,
    code(cxt) {
      const { keyword, data, schemaCode } = cxt;
      const op = keyword === "maxItems" ? codegen_1.operators.GT : codegen_1.operators.LT;
      cxt.fail$data((0, codegen_1._)`${data}.length ${op} ${schemaCode}`);
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/runtime/equal.js
var require_equal = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var equal = require_fast_deep_equal();
  equal.code = 'require("ajv/dist/runtime/equal").default';
  exports.default = equal;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/validation/uniqueItems.js
var require_uniqueItems = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var dataType_1 = require_dataType();
  var codegen_1 = require_codegen();
  var util_1 = require_util();
  var equal_1 = require_equal();
  var error = {
    message: ({ params: { i, j } }) => (0, codegen_1.str)`must NOT have duplicate items (items ## ${j} and ${i} are identical)`,
    params: ({ params: { i, j } }) => (0, codegen_1._)`{i: ${i}, j: ${j}}`
  };
  var def = {
    keyword: "uniqueItems",
    type: "array",
    schemaType: "boolean",
    $data: true,
    error,
    code(cxt) {
      const { gen, data, $data, schema, parentSchema, schemaCode, it } = cxt;
      if (!$data && !schema)
        return;
      const valid = gen.let("valid");
      const itemTypes = parentSchema.items ? (0, dataType_1.getSchemaTypes)(parentSchema.items) : [];
      cxt.block$data(valid, validateUniqueItems, (0, codegen_1._)`${schemaCode} === false`);
      cxt.ok(valid);
      function validateUniqueItems() {
        const i = gen.let("i", (0, codegen_1._)`${data}.length`);
        const j = gen.let("j");
        cxt.setParams({ i, j });
        gen.assign(valid, true);
        gen.if((0, codegen_1._)`${i} > 1`, () => (canOptimize() ? loopN : loopN2)(i, j));
      }
      function canOptimize() {
        return itemTypes.length > 0 && !itemTypes.some((t) => t === "object" || t === "array");
      }
      function loopN(i, j) {
        const item = gen.name("item");
        const wrongType = (0, dataType_1.checkDataTypes)(itemTypes, item, it.opts.strictNumbers, dataType_1.DataType.Wrong);
        const indices = gen.const("indices", (0, codegen_1._)`{}`);
        gen.for((0, codegen_1._)`;${i}--;`, () => {
          gen.let(item, (0, codegen_1._)`${data}[${i}]`);
          gen.if(wrongType, (0, codegen_1._)`continue`);
          if (itemTypes.length > 1)
            gen.if((0, codegen_1._)`typeof ${item} == "string"`, (0, codegen_1._)`${item} += "_"`);
          gen.if((0, codegen_1._)`typeof ${indices}[${item}] == "number"`, () => {
            gen.assign(j, (0, codegen_1._)`${indices}[${item}]`);
            cxt.error();
            gen.assign(valid, false).break();
          }).code((0, codegen_1._)`${indices}[${item}] = ${i}`);
        });
      }
      function loopN2(i, j) {
        const eql = (0, util_1.useFunc)(gen, equal_1.default);
        const outer = gen.name("outer");
        gen.label(outer).for((0, codegen_1._)`;${i}--;`, () => gen.for((0, codegen_1._)`${j} = ${i}; ${j}--;`, () => gen.if((0, codegen_1._)`${eql}(${data}[${i}], ${data}[${j}])`, () => {
          cxt.error();
          gen.assign(valid, false).break(outer);
        })));
      }
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/validation/const.js
var require_const = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var codegen_1 = require_codegen();
  var util_1 = require_util();
  var equal_1 = require_equal();
  var error = {
    message: "must be equal to constant",
    params: ({ schemaCode }) => (0, codegen_1._)`{allowedValue: ${schemaCode}}`
  };
  var def = {
    keyword: "const",
    $data: true,
    error,
    code(cxt) {
      const { gen, data, $data, schemaCode, schema } = cxt;
      if ($data || schema && typeof schema == "object") {
        cxt.fail$data((0, codegen_1._)`!${(0, util_1.useFunc)(gen, equal_1.default)}(${data}, ${schemaCode})`);
      } else {
        cxt.fail((0, codegen_1._)`${schema} !== ${data}`);
      }
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/validation/enum.js
var require_enum = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var codegen_1 = require_codegen();
  var util_1 = require_util();
  var equal_1 = require_equal();
  var error = {
    message: "must be equal to one of the allowed values",
    params: ({ schemaCode }) => (0, codegen_1._)`{allowedValues: ${schemaCode}}`
  };
  var def = {
    keyword: "enum",
    schemaType: "array",
    $data: true,
    error,
    code(cxt) {
      const { gen, data, $data, schema, schemaCode, it } = cxt;
      if (!$data && schema.length === 0)
        throw new Error("enum must have non-empty array");
      const useLoop = schema.length >= it.opts.loopEnum;
      let eql;
      const getEql = () => eql !== null && eql !== undefined ? eql : eql = (0, util_1.useFunc)(gen, equal_1.default);
      let valid;
      if (useLoop || $data) {
        valid = gen.let("valid");
        cxt.block$data(valid, loopEnum);
      } else {
        if (!Array.isArray(schema))
          throw new Error("ajv implementation error");
        const vSchema = gen.const("vSchema", schemaCode);
        valid = (0, codegen_1.or)(...schema.map((_x, i) => equalCode(vSchema, i)));
      }
      cxt.pass(valid);
      function loopEnum() {
        gen.assign(valid, false);
        gen.forOf("v", schemaCode, (v) => gen.if((0, codegen_1._)`${getEql()}(${data}, ${v})`, () => gen.assign(valid, true).break()));
      }
      function equalCode(vSchema, i) {
        const sch = schema[i];
        return typeof sch === "object" && sch !== null ? (0, codegen_1._)`${getEql()}(${data}, ${vSchema}[${i}])` : (0, codegen_1._)`${data} === ${sch}`;
      }
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/validation/index.js
var require_validation = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var limitNumber_1 = require_limitNumber();
  var multipleOf_1 = require_multipleOf();
  var limitLength_1 = require_limitLength();
  var pattern_1 = require_pattern();
  var limitProperties_1 = require_limitProperties();
  var required_1 = require_required();
  var limitItems_1 = require_limitItems();
  var uniqueItems_1 = require_uniqueItems();
  var const_1 = require_const();
  var enum_1 = require_enum();
  var validation = [
    limitNumber_1.default,
    multipleOf_1.default,
    limitLength_1.default,
    pattern_1.default,
    limitProperties_1.default,
    required_1.default,
    limitItems_1.default,
    uniqueItems_1.default,
    { keyword: "type", schemaType: ["string", "array"] },
    { keyword: "nullable", schemaType: "boolean" },
    const_1.default,
    enum_1.default
  ];
  exports.default = validation;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/applicator/additionalItems.js
var require_additionalItems = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.validateAdditionalItems = undefined;
  var codegen_1 = require_codegen();
  var util_1 = require_util();
  var error = {
    message: ({ params: { len } }) => (0, codegen_1.str)`must NOT have more than ${len} items`,
    params: ({ params: { len } }) => (0, codegen_1._)`{limit: ${len}}`
  };
  var def = {
    keyword: "additionalItems",
    type: "array",
    schemaType: ["boolean", "object"],
    before: "uniqueItems",
    error,
    code(cxt) {
      const { parentSchema, it } = cxt;
      const { items } = parentSchema;
      if (!Array.isArray(items)) {
        (0, util_1.checkStrictMode)(it, '"additionalItems" is ignored when "items" is not an array of schemas');
        return;
      }
      validateAdditionalItems(cxt, items);
    }
  };
  function validateAdditionalItems(cxt, items) {
    const { gen, schema, data, keyword, it } = cxt;
    it.items = true;
    const len = gen.const("len", (0, codegen_1._)`${data}.length`);
    if (schema === false) {
      cxt.setParams({ len: items.length });
      cxt.pass((0, codegen_1._)`${len} <= ${items.length}`);
    } else if (typeof schema == "object" && !(0, util_1.alwaysValidSchema)(it, schema)) {
      const valid = gen.var("valid", (0, codegen_1._)`${len} <= ${items.length}`);
      gen.if((0, codegen_1.not)(valid), () => validateItems(valid));
      cxt.ok(valid);
    }
    function validateItems(valid) {
      gen.forRange("i", items.length, len, (i) => {
        cxt.subschema({ keyword, dataProp: i, dataPropType: util_1.Type.Num }, valid);
        if (!it.allErrors)
          gen.if((0, codegen_1.not)(valid), () => gen.break());
      });
    }
  }
  exports.validateAdditionalItems = validateAdditionalItems;
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/applicator/items.js
var require_items = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.validateTuple = undefined;
  var codegen_1 = require_codegen();
  var util_1 = require_util();
  var code_1 = require_code2();
  var def = {
    keyword: "items",
    type: "array",
    schemaType: ["object", "array", "boolean"],
    before: "uniqueItems",
    code(cxt) {
      const { schema, it } = cxt;
      if (Array.isArray(schema))
        return validateTuple(cxt, "additionalItems", schema);
      it.items = true;
      if ((0, util_1.alwaysValidSchema)(it, schema))
        return;
      cxt.ok((0, code_1.validateArray)(cxt));
    }
  };
  function validateTuple(cxt, extraItems, schArr = cxt.schema) {
    const { gen, parentSchema, data, keyword, it } = cxt;
    checkStrictTuple(parentSchema);
    if (it.opts.unevaluated && schArr.length && it.items !== true) {
      it.items = util_1.mergeEvaluated.items(gen, schArr.length, it.items);
    }
    const valid = gen.name("valid");
    const len = gen.const("len", (0, codegen_1._)`${data}.length`);
    schArr.forEach((sch, i) => {
      if ((0, util_1.alwaysValidSchema)(it, sch))
        return;
      gen.if((0, codegen_1._)`${len} > ${i}`, () => cxt.subschema({
        keyword,
        schemaProp: i,
        dataProp: i
      }, valid));
      cxt.ok(valid);
    });
    function checkStrictTuple(sch) {
      const { opts, errSchemaPath } = it;
      const l = schArr.length;
      const fullTuple = l === sch.minItems && (l === sch.maxItems || sch[extraItems] === false);
      if (opts.strictTuples && !fullTuple) {
        const msg = `"${keyword}" is ${l}-tuple, but minItems or maxItems/${extraItems} are not specified or different at path "${errSchemaPath}"`;
        (0, util_1.checkStrictMode)(it, msg, opts.strictTuples);
      }
    }
  }
  exports.validateTuple = validateTuple;
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/applicator/prefixItems.js
var require_prefixItems = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var items_1 = require_items();
  var def = {
    keyword: "prefixItems",
    type: "array",
    schemaType: ["array"],
    before: "uniqueItems",
    code: (cxt) => (0, items_1.validateTuple)(cxt, "items")
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/applicator/items2020.js
var require_items2020 = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var codegen_1 = require_codegen();
  var util_1 = require_util();
  var code_1 = require_code2();
  var additionalItems_1 = require_additionalItems();
  var error = {
    message: ({ params: { len } }) => (0, codegen_1.str)`must NOT have more than ${len} items`,
    params: ({ params: { len } }) => (0, codegen_1._)`{limit: ${len}}`
  };
  var def = {
    keyword: "items",
    type: "array",
    schemaType: ["object", "boolean"],
    before: "uniqueItems",
    error,
    code(cxt) {
      const { schema, parentSchema, it } = cxt;
      const { prefixItems } = parentSchema;
      it.items = true;
      if ((0, util_1.alwaysValidSchema)(it, schema))
        return;
      if (prefixItems)
        (0, additionalItems_1.validateAdditionalItems)(cxt, prefixItems);
      else
        cxt.ok((0, code_1.validateArray)(cxt));
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/applicator/contains.js
var require_contains = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var codegen_1 = require_codegen();
  var util_1 = require_util();
  var error = {
    message: ({ params: { min, max } }) => max === undefined ? (0, codegen_1.str)`must contain at least ${min} valid item(s)` : (0, codegen_1.str)`must contain at least ${min} and no more than ${max} valid item(s)`,
    params: ({ params: { min, max } }) => max === undefined ? (0, codegen_1._)`{minContains: ${min}}` : (0, codegen_1._)`{minContains: ${min}, maxContains: ${max}}`
  };
  var def = {
    keyword: "contains",
    type: "array",
    schemaType: ["object", "boolean"],
    before: "uniqueItems",
    trackErrors: true,
    error,
    code(cxt) {
      const { gen, schema, parentSchema, data, it } = cxt;
      let min;
      let max;
      const { minContains, maxContains } = parentSchema;
      if (it.opts.next) {
        min = minContains === undefined ? 1 : minContains;
        max = maxContains;
      } else {
        min = 1;
      }
      const len = gen.const("len", (0, codegen_1._)`${data}.length`);
      cxt.setParams({ min, max });
      if (max === undefined && min === 0) {
        (0, util_1.checkStrictMode)(it, `"minContains" == 0 without "maxContains": "contains" keyword ignored`);
        return;
      }
      if (max !== undefined && min > max) {
        (0, util_1.checkStrictMode)(it, `"minContains" > "maxContains" is always invalid`);
        cxt.fail();
        return;
      }
      if ((0, util_1.alwaysValidSchema)(it, schema)) {
        let cond = (0, codegen_1._)`${len} >= ${min}`;
        if (max !== undefined)
          cond = (0, codegen_1._)`${cond} && ${len} <= ${max}`;
        cxt.pass(cond);
        return;
      }
      it.items = true;
      const valid = gen.name("valid");
      if (max === undefined && min === 1) {
        validateItems(valid, () => gen.if(valid, () => gen.break()));
      } else if (min === 0) {
        gen.let(valid, true);
        if (max !== undefined)
          gen.if((0, codegen_1._)`${data}.length > 0`, validateItemsWithCount);
      } else {
        gen.let(valid, false);
        validateItemsWithCount();
      }
      cxt.result(valid, () => cxt.reset());
      function validateItemsWithCount() {
        const schValid = gen.name("_valid");
        const count = gen.let("count", 0);
        validateItems(schValid, () => gen.if(schValid, () => checkLimits(count)));
      }
      function validateItems(_valid, block) {
        gen.forRange("i", 0, len, (i) => {
          cxt.subschema({
            keyword: "contains",
            dataProp: i,
            dataPropType: util_1.Type.Num,
            compositeRule: true
          }, _valid);
          block();
        });
      }
      function checkLimits(count) {
        gen.code((0, codegen_1._)`${count}++`);
        if (max === undefined) {
          gen.if((0, codegen_1._)`${count} >= ${min}`, () => gen.assign(valid, true).break());
        } else {
          gen.if((0, codegen_1._)`${count} > ${max}`, () => gen.assign(valid, false).break());
          if (min === 1)
            gen.assign(valid, true);
          else
            gen.if((0, codegen_1._)`${count} >= ${min}`, () => gen.assign(valid, true));
        }
      }
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/applicator/dependencies.js
var require_dependencies = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.validateSchemaDeps = exports.validatePropertyDeps = exports.error = undefined;
  var codegen_1 = require_codegen();
  var util_1 = require_util();
  var code_1 = require_code2();
  exports.error = {
    message: ({ params: { property, depsCount, deps } }) => {
      const property_ies = depsCount === 1 ? "property" : "properties";
      return (0, codegen_1.str)`must have ${property_ies} ${deps} when property ${property} is present`;
    },
    params: ({ params: { property, depsCount, deps, missingProperty } }) => (0, codegen_1._)`{property: ${property},
    missingProperty: ${missingProperty},
    depsCount: ${depsCount},
    deps: ${deps}}`
  };
  var def = {
    keyword: "dependencies",
    type: "object",
    schemaType: "object",
    error: exports.error,
    code(cxt) {
      const [propDeps, schDeps] = splitDependencies(cxt);
      validatePropertyDeps(cxt, propDeps);
      validateSchemaDeps(cxt, schDeps);
    }
  };
  function splitDependencies({ schema }) {
    const propertyDeps = {};
    const schemaDeps = {};
    for (const key in schema) {
      if (key === "__proto__")
        continue;
      const deps = Array.isArray(schema[key]) ? propertyDeps : schemaDeps;
      deps[key] = schema[key];
    }
    return [propertyDeps, schemaDeps];
  }
  function validatePropertyDeps(cxt, propertyDeps = cxt.schema) {
    const { gen, data, it } = cxt;
    if (Object.keys(propertyDeps).length === 0)
      return;
    const missing = gen.let("missing");
    for (const prop in propertyDeps) {
      const deps = propertyDeps[prop];
      if (deps.length === 0)
        continue;
      const hasProperty = (0, code_1.propertyInData)(gen, data, prop, it.opts.ownProperties);
      cxt.setParams({
        property: prop,
        depsCount: deps.length,
        deps: deps.join(", ")
      });
      if (it.allErrors) {
        gen.if(hasProperty, () => {
          for (const depProp of deps) {
            (0, code_1.checkReportMissingProp)(cxt, depProp);
          }
        });
      } else {
        gen.if((0, codegen_1._)`${hasProperty} && (${(0, code_1.checkMissingProp)(cxt, deps, missing)})`);
        (0, code_1.reportMissingProp)(cxt, missing);
        gen.else();
      }
    }
  }
  exports.validatePropertyDeps = validatePropertyDeps;
  function validateSchemaDeps(cxt, schemaDeps = cxt.schema) {
    const { gen, data, keyword, it } = cxt;
    const valid = gen.name("valid");
    for (const prop in schemaDeps) {
      if ((0, util_1.alwaysValidSchema)(it, schemaDeps[prop]))
        continue;
      gen.if((0, code_1.propertyInData)(gen, data, prop, it.opts.ownProperties), () => {
        const schCxt = cxt.subschema({ keyword, schemaProp: prop }, valid);
        cxt.mergeValidEvaluated(schCxt, valid);
      }, () => gen.var(valid, true));
      cxt.ok(valid);
    }
  }
  exports.validateSchemaDeps = validateSchemaDeps;
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/applicator/propertyNames.js
var require_propertyNames = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var codegen_1 = require_codegen();
  var util_1 = require_util();
  var error = {
    message: "property name must be valid",
    params: ({ params }) => (0, codegen_1._)`{propertyName: ${params.propertyName}}`
  };
  var def = {
    keyword: "propertyNames",
    type: "object",
    schemaType: ["object", "boolean"],
    error,
    code(cxt) {
      const { gen, schema, data, it } = cxt;
      if ((0, util_1.alwaysValidSchema)(it, schema))
        return;
      const valid = gen.name("valid");
      gen.forIn("key", data, (key) => {
        cxt.setParams({ propertyName: key });
        cxt.subschema({
          keyword: "propertyNames",
          data: key,
          dataTypes: ["string"],
          propertyName: key,
          compositeRule: true
        }, valid);
        gen.if((0, codegen_1.not)(valid), () => {
          cxt.error(true);
          if (!it.allErrors)
            gen.break();
        });
      });
      cxt.ok(valid);
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/applicator/additionalProperties.js
var require_additionalProperties = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var code_1 = require_code2();
  var codegen_1 = require_codegen();
  var names_1 = require_names();
  var util_1 = require_util();
  var error = {
    message: "must NOT have additional properties",
    params: ({ params }) => (0, codegen_1._)`{additionalProperty: ${params.additionalProperty}}`
  };
  var def = {
    keyword: "additionalProperties",
    type: ["object"],
    schemaType: ["boolean", "object"],
    allowUndefined: true,
    trackErrors: true,
    error,
    code(cxt) {
      const { gen, schema, parentSchema, data, errsCount, it } = cxt;
      if (!errsCount)
        throw new Error("ajv implementation error");
      const { allErrors, opts } = it;
      it.props = true;
      if (opts.removeAdditional !== "all" && (0, util_1.alwaysValidSchema)(it, schema))
        return;
      const props = (0, code_1.allSchemaProperties)(parentSchema.properties);
      const patProps = (0, code_1.allSchemaProperties)(parentSchema.patternProperties);
      checkAdditionalProperties();
      cxt.ok((0, codegen_1._)`${errsCount} === ${names_1.default.errors}`);
      function checkAdditionalProperties() {
        gen.forIn("key", data, (key) => {
          if (!props.length && !patProps.length)
            additionalPropertyCode(key);
          else
            gen.if(isAdditional(key), () => additionalPropertyCode(key));
        });
      }
      function isAdditional(key) {
        let definedProp;
        if (props.length > 8) {
          const propsSchema = (0, util_1.schemaRefOrVal)(it, parentSchema.properties, "properties");
          definedProp = (0, code_1.isOwnProperty)(gen, propsSchema, key);
        } else if (props.length) {
          definedProp = (0, codegen_1.or)(...props.map((p) => (0, codegen_1._)`${key} === ${p}`));
        } else {
          definedProp = codegen_1.nil;
        }
        if (patProps.length) {
          definedProp = (0, codegen_1.or)(definedProp, ...patProps.map((p) => (0, codegen_1._)`${(0, code_1.usePattern)(cxt, p)}.test(${key})`));
        }
        return (0, codegen_1.not)(definedProp);
      }
      function deleteAdditional(key) {
        gen.code((0, codegen_1._)`delete ${data}[${key}]`);
      }
      function additionalPropertyCode(key) {
        if (opts.removeAdditional === "all" || opts.removeAdditional && schema === false) {
          deleteAdditional(key);
          return;
        }
        if (schema === false) {
          cxt.setParams({ additionalProperty: key });
          cxt.error();
          if (!allErrors)
            gen.break();
          return;
        }
        if (typeof schema == "object" && !(0, util_1.alwaysValidSchema)(it, schema)) {
          const valid = gen.name("valid");
          if (opts.removeAdditional === "failing") {
            applyAdditionalSchema(key, valid, false);
            gen.if((0, codegen_1.not)(valid), () => {
              cxt.reset();
              deleteAdditional(key);
            });
          } else {
            applyAdditionalSchema(key, valid);
            if (!allErrors)
              gen.if((0, codegen_1.not)(valid), () => gen.break());
          }
        }
      }
      function applyAdditionalSchema(key, valid, errors) {
        const subschema = {
          keyword: "additionalProperties",
          dataProp: key,
          dataPropType: util_1.Type.Str
        };
        if (errors === false) {
          Object.assign(subschema, {
            compositeRule: true,
            createErrors: false,
            allErrors: false
          });
        }
        cxt.subschema(subschema, valid);
      }
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/applicator/properties.js
var require_properties = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var validate_1 = require_validate();
  var code_1 = require_code2();
  var util_1 = require_util();
  var additionalProperties_1 = require_additionalProperties();
  var def = {
    keyword: "properties",
    type: "object",
    schemaType: "object",
    code(cxt) {
      const { gen, schema, parentSchema, data, it } = cxt;
      if (it.opts.removeAdditional === "all" && parentSchema.additionalProperties === undefined) {
        additionalProperties_1.default.code(new validate_1.KeywordCxt(it, additionalProperties_1.default, "additionalProperties"));
      }
      const allProps = (0, code_1.allSchemaProperties)(schema);
      for (const prop of allProps) {
        it.definedProperties.add(prop);
      }
      if (it.opts.unevaluated && allProps.length && it.props !== true) {
        it.props = util_1.mergeEvaluated.props(gen, (0, util_1.toHash)(allProps), it.props);
      }
      const properties = allProps.filter((p) => !(0, util_1.alwaysValidSchema)(it, schema[p]));
      if (properties.length === 0)
        return;
      const valid = gen.name("valid");
      for (const prop of properties) {
        if (hasDefault(prop)) {
          applyPropertySchema(prop);
        } else {
          gen.if((0, code_1.propertyInData)(gen, data, prop, it.opts.ownProperties));
          applyPropertySchema(prop);
          if (!it.allErrors)
            gen.else().var(valid, true);
          gen.endIf();
        }
        cxt.it.definedProperties.add(prop);
        cxt.ok(valid);
      }
      function hasDefault(prop) {
        return it.opts.useDefaults && !it.compositeRule && schema[prop].default !== undefined;
      }
      function applyPropertySchema(prop) {
        cxt.subschema({
          keyword: "properties",
          schemaProp: prop,
          dataProp: prop
        }, valid);
      }
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/applicator/patternProperties.js
var require_patternProperties = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var code_1 = require_code2();
  var codegen_1 = require_codegen();
  var util_1 = require_util();
  var util_2 = require_util();
  var def = {
    keyword: "patternProperties",
    type: "object",
    schemaType: "object",
    code(cxt) {
      const { gen, schema, data, parentSchema, it } = cxt;
      const { opts } = it;
      const patterns = (0, code_1.allSchemaProperties)(schema);
      const alwaysValidPatterns = patterns.filter((p) => (0, util_1.alwaysValidSchema)(it, schema[p]));
      if (patterns.length === 0 || alwaysValidPatterns.length === patterns.length && (!it.opts.unevaluated || it.props === true)) {
        return;
      }
      const checkProperties = opts.strictSchema && !opts.allowMatchingProperties && parentSchema.properties;
      const valid = gen.name("valid");
      if (it.props !== true && !(it.props instanceof codegen_1.Name)) {
        it.props = (0, util_2.evaluatedPropsToName)(gen, it.props);
      }
      const { props } = it;
      validatePatternProperties();
      function validatePatternProperties() {
        for (const pat of patterns) {
          if (checkProperties)
            checkMatchingProperties(pat);
          if (it.allErrors) {
            validateProperties(pat);
          } else {
            gen.var(valid, true);
            validateProperties(pat);
            gen.if(valid);
          }
        }
      }
      function checkMatchingProperties(pat) {
        for (const prop in checkProperties) {
          if (new RegExp(pat).test(prop)) {
            (0, util_1.checkStrictMode)(it, `property ${prop} matches pattern ${pat} (use allowMatchingProperties)`);
          }
        }
      }
      function validateProperties(pat) {
        gen.forIn("key", data, (key) => {
          gen.if((0, codegen_1._)`${(0, code_1.usePattern)(cxt, pat)}.test(${key})`, () => {
            const alwaysValid = alwaysValidPatterns.includes(pat);
            if (!alwaysValid) {
              cxt.subschema({
                keyword: "patternProperties",
                schemaProp: pat,
                dataProp: key,
                dataPropType: util_2.Type.Str
              }, valid);
            }
            if (it.opts.unevaluated && props !== true) {
              gen.assign((0, codegen_1._)`${props}[${key}]`, true);
            } else if (!alwaysValid && !it.allErrors) {
              gen.if((0, codegen_1.not)(valid), () => gen.break());
            }
          });
        });
      }
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/applicator/not.js
var require_not = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var util_1 = require_util();
  var def = {
    keyword: "not",
    schemaType: ["object", "boolean"],
    trackErrors: true,
    code(cxt) {
      const { gen, schema, it } = cxt;
      if ((0, util_1.alwaysValidSchema)(it, schema)) {
        cxt.fail();
        return;
      }
      const valid = gen.name("valid");
      cxt.subschema({
        keyword: "not",
        compositeRule: true,
        createErrors: false,
        allErrors: false
      }, valid);
      cxt.failResult(valid, () => cxt.reset(), () => cxt.error());
    },
    error: { message: "must NOT be valid" }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/applicator/anyOf.js
var require_anyOf = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var code_1 = require_code2();
  var def = {
    keyword: "anyOf",
    schemaType: "array",
    trackErrors: true,
    code: code_1.validateUnion,
    error: { message: "must match a schema in anyOf" }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/applicator/oneOf.js
var require_oneOf = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var codegen_1 = require_codegen();
  var util_1 = require_util();
  var error = {
    message: "must match exactly one schema in oneOf",
    params: ({ params }) => (0, codegen_1._)`{passingSchemas: ${params.passing}}`
  };
  var def = {
    keyword: "oneOf",
    schemaType: "array",
    trackErrors: true,
    error,
    code(cxt) {
      const { gen, schema, parentSchema, it } = cxt;
      if (!Array.isArray(schema))
        throw new Error("ajv implementation error");
      if (it.opts.discriminator && parentSchema.discriminator)
        return;
      const schArr = schema;
      const valid = gen.let("valid", false);
      const passing = gen.let("passing", null);
      const schValid = gen.name("_valid");
      cxt.setParams({ passing });
      gen.block(validateOneOf);
      cxt.result(valid, () => cxt.reset(), () => cxt.error(true));
      function validateOneOf() {
        schArr.forEach((sch, i) => {
          let schCxt;
          if ((0, util_1.alwaysValidSchema)(it, sch)) {
            gen.var(schValid, true);
          } else {
            schCxt = cxt.subschema({
              keyword: "oneOf",
              schemaProp: i,
              compositeRule: true
            }, schValid);
          }
          if (i > 0) {
            gen.if((0, codegen_1._)`${schValid} && ${valid}`).assign(valid, false).assign(passing, (0, codegen_1._)`[${passing}, ${i}]`).else();
          }
          gen.if(schValid, () => {
            gen.assign(valid, true);
            gen.assign(passing, i);
            if (schCxt)
              cxt.mergeEvaluated(schCxt, codegen_1.Name);
          });
        });
      }
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/applicator/allOf.js
var require_allOf = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var util_1 = require_util();
  var def = {
    keyword: "allOf",
    schemaType: "array",
    code(cxt) {
      const { gen, schema, it } = cxt;
      if (!Array.isArray(schema))
        throw new Error("ajv implementation error");
      const valid = gen.name("valid");
      schema.forEach((sch, i) => {
        if ((0, util_1.alwaysValidSchema)(it, sch))
          return;
        const schCxt = cxt.subschema({ keyword: "allOf", schemaProp: i }, valid);
        cxt.ok(valid);
        cxt.mergeEvaluated(schCxt);
      });
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/applicator/if.js
var require_if = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var codegen_1 = require_codegen();
  var util_1 = require_util();
  var error = {
    message: ({ params }) => (0, codegen_1.str)`must match "${params.ifClause}" schema`,
    params: ({ params }) => (0, codegen_1._)`{failingKeyword: ${params.ifClause}}`
  };
  var def = {
    keyword: "if",
    schemaType: ["object", "boolean"],
    trackErrors: true,
    error,
    code(cxt) {
      const { gen, parentSchema, it } = cxt;
      if (parentSchema.then === undefined && parentSchema.else === undefined) {
        (0, util_1.checkStrictMode)(it, '"if" without "then" and "else" is ignored');
      }
      const hasThen = hasSchema(it, "then");
      const hasElse = hasSchema(it, "else");
      if (!hasThen && !hasElse)
        return;
      const valid = gen.let("valid", true);
      const schValid = gen.name("_valid");
      validateIf();
      cxt.reset();
      if (hasThen && hasElse) {
        const ifClause = gen.let("ifClause");
        cxt.setParams({ ifClause });
        gen.if(schValid, validateClause("then", ifClause), validateClause("else", ifClause));
      } else if (hasThen) {
        gen.if(schValid, validateClause("then"));
      } else {
        gen.if((0, codegen_1.not)(schValid), validateClause("else"));
      }
      cxt.pass(valid, () => cxt.error(true));
      function validateIf() {
        const schCxt = cxt.subschema({
          keyword: "if",
          compositeRule: true,
          createErrors: false,
          allErrors: false
        }, schValid);
        cxt.mergeEvaluated(schCxt);
      }
      function validateClause(keyword, ifClause) {
        return () => {
          const schCxt = cxt.subschema({ keyword }, schValid);
          gen.assign(valid, schValid);
          cxt.mergeValidEvaluated(schCxt, valid);
          if (ifClause)
            gen.assign(ifClause, (0, codegen_1._)`${keyword}`);
          else
            cxt.setParams({ ifClause: keyword });
        };
      }
    }
  };
  function hasSchema(it, keyword) {
    const schema = it.schema[keyword];
    return schema !== undefined && !(0, util_1.alwaysValidSchema)(it, schema);
  }
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/applicator/thenElse.js
var require_thenElse = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var util_1 = require_util();
  var def = {
    keyword: ["then", "else"],
    schemaType: ["object", "boolean"],
    code({ keyword, parentSchema, it }) {
      if (parentSchema.if === undefined)
        (0, util_1.checkStrictMode)(it, `"${keyword}" without "if" is ignored`);
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/applicator/index.js
var require_applicator = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var additionalItems_1 = require_additionalItems();
  var prefixItems_1 = require_prefixItems();
  var items_1 = require_items();
  var items2020_1 = require_items2020();
  var contains_1 = require_contains();
  var dependencies_1 = require_dependencies();
  var propertyNames_1 = require_propertyNames();
  var additionalProperties_1 = require_additionalProperties();
  var properties_1 = require_properties();
  var patternProperties_1 = require_patternProperties();
  var not_1 = require_not();
  var anyOf_1 = require_anyOf();
  var oneOf_1 = require_oneOf();
  var allOf_1 = require_allOf();
  var if_1 = require_if();
  var thenElse_1 = require_thenElse();
  function getApplicator(draft2020 = false) {
    const applicator = [
      not_1.default,
      anyOf_1.default,
      oneOf_1.default,
      allOf_1.default,
      if_1.default,
      thenElse_1.default,
      propertyNames_1.default,
      additionalProperties_1.default,
      dependencies_1.default,
      properties_1.default,
      patternProperties_1.default
    ];
    if (draft2020)
      applicator.push(prefixItems_1.default, items2020_1.default);
    else
      applicator.push(additionalItems_1.default, items_1.default);
    applicator.push(contains_1.default);
    return applicator;
  }
  exports.default = getApplicator;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/dynamic/dynamicAnchor.js
var require_dynamicAnchor = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.dynamicAnchor = undefined;
  var codegen_1 = require_codegen();
  var names_1 = require_names();
  var compile_1 = require_compile();
  var ref_1 = require_ref();
  var def = {
    keyword: "$dynamicAnchor",
    schemaType: "string",
    code: (cxt) => dynamicAnchor(cxt, cxt.schema)
  };
  function dynamicAnchor(cxt, anchor) {
    const { gen, it } = cxt;
    it.schemaEnv.root.dynamicAnchors[anchor] = true;
    const v = (0, codegen_1._)`${names_1.default.dynamicAnchors}${(0, codegen_1.getProperty)(anchor)}`;
    const validate = it.errSchemaPath === "#" ? it.validateName : _getValidate(cxt);
    gen.if((0, codegen_1._)`!${v}`, () => gen.assign(v, validate));
  }
  exports.dynamicAnchor = dynamicAnchor;
  function _getValidate(cxt) {
    const { schemaEnv, schema, self } = cxt.it;
    const { root, baseId, localRefs, meta } = schemaEnv.root;
    const { schemaId } = self.opts;
    const sch = new compile_1.SchemaEnv({ schema, schemaId, root, baseId, localRefs, meta });
    compile_1.compileSchema.call(self, sch);
    return (0, ref_1.getValidate)(cxt, sch);
  }
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/dynamic/dynamicRef.js
var require_dynamicRef = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.dynamicRef = undefined;
  var codegen_1 = require_codegen();
  var names_1 = require_names();
  var ref_1 = require_ref();
  var def = {
    keyword: "$dynamicRef",
    schemaType: "string",
    code: (cxt) => dynamicRef(cxt, cxt.schema)
  };
  function dynamicRef(cxt, ref) {
    const { gen, keyword, it } = cxt;
    if (ref[0] !== "#")
      throw new Error(`"${keyword}" only supports hash fragment reference`);
    const anchor = ref.slice(1);
    if (it.allErrors) {
      _dynamicRef();
    } else {
      const valid = gen.let("valid", false);
      _dynamicRef(valid);
      cxt.ok(valid);
    }
    function _dynamicRef(valid) {
      if (it.schemaEnv.root.dynamicAnchors[anchor]) {
        const v = gen.let("_v", (0, codegen_1._)`${names_1.default.dynamicAnchors}${(0, codegen_1.getProperty)(anchor)}`);
        gen.if(v, _callRef(v, valid), _callRef(it.validateName, valid));
      } else {
        _callRef(it.validateName, valid)();
      }
    }
    function _callRef(validate, valid) {
      return valid ? () => gen.block(() => {
        (0, ref_1.callRef)(cxt, validate);
        gen.let(valid, true);
      }) : () => (0, ref_1.callRef)(cxt, validate);
    }
  }
  exports.dynamicRef = dynamicRef;
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/dynamic/recursiveAnchor.js
var require_recursiveAnchor = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var dynamicAnchor_1 = require_dynamicAnchor();
  var util_1 = require_util();
  var def = {
    keyword: "$recursiveAnchor",
    schemaType: "boolean",
    code(cxt) {
      if (cxt.schema)
        (0, dynamicAnchor_1.dynamicAnchor)(cxt, "");
      else
        (0, util_1.checkStrictMode)(cxt.it, "$recursiveAnchor: false is ignored");
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/dynamic/recursiveRef.js
var require_recursiveRef = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var dynamicRef_1 = require_dynamicRef();
  var def = {
    keyword: "$recursiveRef",
    schemaType: "string",
    code: (cxt) => (0, dynamicRef_1.dynamicRef)(cxt, cxt.schema)
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/dynamic/index.js
var require_dynamic = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var dynamicAnchor_1 = require_dynamicAnchor();
  var dynamicRef_1 = require_dynamicRef();
  var recursiveAnchor_1 = require_recursiveAnchor();
  var recursiveRef_1 = require_recursiveRef();
  var dynamic = [dynamicAnchor_1.default, dynamicRef_1.default, recursiveAnchor_1.default, recursiveRef_1.default];
  exports.default = dynamic;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/validation/dependentRequired.js
var require_dependentRequired = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var dependencies_1 = require_dependencies();
  var def = {
    keyword: "dependentRequired",
    type: "object",
    schemaType: "object",
    error: dependencies_1.error,
    code: (cxt) => (0, dependencies_1.validatePropertyDeps)(cxt)
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/applicator/dependentSchemas.js
var require_dependentSchemas = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var dependencies_1 = require_dependencies();
  var def = {
    keyword: "dependentSchemas",
    type: "object",
    schemaType: "object",
    code: (cxt) => (0, dependencies_1.validateSchemaDeps)(cxt)
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/validation/limitContains.js
var require_limitContains = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var util_1 = require_util();
  var def = {
    keyword: ["maxContains", "minContains"],
    type: "array",
    schemaType: "number",
    code({ keyword, parentSchema, it }) {
      if (parentSchema.contains === undefined) {
        (0, util_1.checkStrictMode)(it, `"${keyword}" without "contains" is ignored`);
      }
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/next.js
var require_next = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var dependentRequired_1 = require_dependentRequired();
  var dependentSchemas_1 = require_dependentSchemas();
  var limitContains_1 = require_limitContains();
  var next = [dependentRequired_1.default, dependentSchemas_1.default, limitContains_1.default];
  exports.default = next;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/unevaluated/unevaluatedProperties.js
var require_unevaluatedProperties = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var codegen_1 = require_codegen();
  var util_1 = require_util();
  var names_1 = require_names();
  var error = {
    message: "must NOT have unevaluated properties",
    params: ({ params }) => (0, codegen_1._)`{unevaluatedProperty: ${params.unevaluatedProperty}}`
  };
  var def = {
    keyword: "unevaluatedProperties",
    type: "object",
    schemaType: ["boolean", "object"],
    trackErrors: true,
    error,
    code(cxt) {
      const { gen, schema, data, errsCount, it } = cxt;
      if (!errsCount)
        throw new Error("ajv implementation error");
      const { allErrors, props } = it;
      if (props instanceof codegen_1.Name) {
        gen.if((0, codegen_1._)`${props} !== true`, () => gen.forIn("key", data, (key) => gen.if(unevaluatedDynamic(props, key), () => unevaluatedPropCode(key))));
      } else if (props !== true) {
        gen.forIn("key", data, (key) => props === undefined ? unevaluatedPropCode(key) : gen.if(unevaluatedStatic(props, key), () => unevaluatedPropCode(key)));
      }
      it.props = true;
      cxt.ok((0, codegen_1._)`${errsCount} === ${names_1.default.errors}`);
      function unevaluatedPropCode(key) {
        if (schema === false) {
          cxt.setParams({ unevaluatedProperty: key });
          cxt.error();
          if (!allErrors)
            gen.break();
          return;
        }
        if (!(0, util_1.alwaysValidSchema)(it, schema)) {
          const valid = gen.name("valid");
          cxt.subschema({
            keyword: "unevaluatedProperties",
            dataProp: key,
            dataPropType: util_1.Type.Str
          }, valid);
          if (!allErrors)
            gen.if((0, codegen_1.not)(valid), () => gen.break());
        }
      }
      function unevaluatedDynamic(evaluatedProps, key) {
        return (0, codegen_1._)`!${evaluatedProps} || !${evaluatedProps}[${key}]`;
      }
      function unevaluatedStatic(evaluatedProps, key) {
        const ps = [];
        for (const p in evaluatedProps) {
          if (evaluatedProps[p] === true)
            ps.push((0, codegen_1._)`${key} !== ${p}`);
        }
        return (0, codegen_1.and)(...ps);
      }
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/unevaluated/unevaluatedItems.js
var require_unevaluatedItems = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var codegen_1 = require_codegen();
  var util_1 = require_util();
  var error = {
    message: ({ params: { len } }) => (0, codegen_1.str)`must NOT have more than ${len} items`,
    params: ({ params: { len } }) => (0, codegen_1._)`{limit: ${len}}`
  };
  var def = {
    keyword: "unevaluatedItems",
    type: "array",
    schemaType: ["boolean", "object"],
    error,
    code(cxt) {
      const { gen, schema, data, it } = cxt;
      const items = it.items || 0;
      if (items === true)
        return;
      const len = gen.const("len", (0, codegen_1._)`${data}.length`);
      if (schema === false) {
        cxt.setParams({ len: items });
        cxt.fail((0, codegen_1._)`${len} > ${items}`);
      } else if (typeof schema == "object" && !(0, util_1.alwaysValidSchema)(it, schema)) {
        const valid = gen.var("valid", (0, codegen_1._)`${len} <= ${items}`);
        gen.if((0, codegen_1.not)(valid), () => validateItems(valid, items));
        cxt.ok(valid);
      }
      it.items = true;
      function validateItems(valid, from) {
        gen.forRange("i", from, len, (i) => {
          cxt.subschema({ keyword: "unevaluatedItems", dataProp: i, dataPropType: util_1.Type.Num }, valid);
          if (!it.allErrors)
            gen.if((0, codegen_1.not)(valid), () => gen.break());
        });
      }
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/unevaluated/index.js
var require_unevaluated = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var unevaluatedProperties_1 = require_unevaluatedProperties();
  var unevaluatedItems_1 = require_unevaluatedItems();
  var unevaluated = [unevaluatedProperties_1.default, unevaluatedItems_1.default];
  exports.default = unevaluated;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/format/format.js
var require_format = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var codegen_1 = require_codegen();
  var error = {
    message: ({ schemaCode }) => (0, codegen_1.str)`must match format "${schemaCode}"`,
    params: ({ schemaCode }) => (0, codegen_1._)`{format: ${schemaCode}}`
  };
  var def = {
    keyword: "format",
    type: ["number", "string"],
    schemaType: "string",
    $data: true,
    error,
    code(cxt, ruleType) {
      const { gen, data, $data, schema, schemaCode, it } = cxt;
      const { opts, errSchemaPath, schemaEnv, self } = it;
      if (!opts.validateFormats)
        return;
      if ($data)
        validate$DataFormat();
      else
        validateFormat();
      function validate$DataFormat() {
        const fmts = gen.scopeValue("formats", {
          ref: self.formats,
          code: opts.code.formats
        });
        const fDef = gen.const("fDef", (0, codegen_1._)`${fmts}[${schemaCode}]`);
        const fType = gen.let("fType");
        const format = gen.let("format");
        gen.if((0, codegen_1._)`typeof ${fDef} == "object" && !(${fDef} instanceof RegExp)`, () => gen.assign(fType, (0, codegen_1._)`${fDef}.type || "string"`).assign(format, (0, codegen_1._)`${fDef}.validate`), () => gen.assign(fType, (0, codegen_1._)`"string"`).assign(format, fDef));
        cxt.fail$data((0, codegen_1.or)(unknownFmt(), invalidFmt()));
        function unknownFmt() {
          if (opts.strictSchema === false)
            return codegen_1.nil;
          return (0, codegen_1._)`${schemaCode} && !${format}`;
        }
        function invalidFmt() {
          const callFormat = schemaEnv.$async ? (0, codegen_1._)`(${fDef}.async ? await ${format}(${data}) : ${format}(${data}))` : (0, codegen_1._)`${format}(${data})`;
          const validData = (0, codegen_1._)`(typeof ${format} == "function" ? ${callFormat} : ${format}.test(${data}))`;
          return (0, codegen_1._)`${format} && ${format} !== true && ${fType} === ${ruleType} && !${validData}`;
        }
      }
      function validateFormat() {
        const formatDef = self.formats[schema];
        if (!formatDef) {
          unknownFormat();
          return;
        }
        if (formatDef === true)
          return;
        const [fmtType, format, fmtRef] = getFormat(formatDef);
        if (fmtType === ruleType)
          cxt.pass(validCondition());
        function unknownFormat() {
          if (opts.strictSchema === false) {
            self.logger.warn(unknownMsg());
            return;
          }
          throw new Error(unknownMsg());
          function unknownMsg() {
            return `unknown format "${schema}" ignored in schema at path "${errSchemaPath}"`;
          }
        }
        function getFormat(fmtDef) {
          const code = fmtDef instanceof RegExp ? (0, codegen_1.regexpCode)(fmtDef) : opts.code.formats ? (0, codegen_1._)`${opts.code.formats}${(0, codegen_1.getProperty)(schema)}` : undefined;
          const fmt = gen.scopeValue("formats", { key: schema, ref: fmtDef, code });
          if (typeof fmtDef == "object" && !(fmtDef instanceof RegExp)) {
            return [fmtDef.type || "string", fmtDef.validate, (0, codegen_1._)`${fmt}.validate`];
          }
          return ["string", fmtDef, fmt];
        }
        function validCondition() {
          if (typeof formatDef == "object" && !(formatDef instanceof RegExp) && formatDef.async) {
            if (!schemaEnv.$async)
              throw new Error("async format in sync schema");
            return (0, codegen_1._)`await ${fmtRef}(${data})`;
          }
          return typeof format == "function" ? (0, codegen_1._)`${fmtRef}(${data})` : (0, codegen_1._)`${fmtRef}.test(${data})`;
        }
      }
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/format/index.js
var require_format2 = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var format_1 = require_format();
  var format = [format_1.default];
  exports.default = format;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/metadata.js
var require_metadata = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.contentVocabulary = exports.metadataVocabulary = undefined;
  exports.metadataVocabulary = [
    "title",
    "description",
    "default",
    "deprecated",
    "readOnly",
    "writeOnly",
    "examples"
  ];
  exports.contentVocabulary = [
    "contentMediaType",
    "contentEncoding",
    "contentSchema"
  ];
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/draft2020.js
var require_draft2020 = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var core_1 = require_core2();
  var validation_1 = require_validation();
  var applicator_1 = require_applicator();
  var dynamic_1 = require_dynamic();
  var next_1 = require_next();
  var unevaluated_1 = require_unevaluated();
  var format_1 = require_format2();
  var metadata_1 = require_metadata();
  var draft2020Vocabularies = [
    dynamic_1.default,
    core_1.default,
    validation_1.default,
    (0, applicator_1.default)(true),
    format_1.default,
    metadata_1.metadataVocabulary,
    metadata_1.contentVocabulary,
    next_1.default,
    unevaluated_1.default
  ];
  exports.default = draft2020Vocabularies;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/discriminator/types.js
var require_types = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.DiscrError = undefined;
  var DiscrError;
  (function(DiscrError2) {
    DiscrError2["Tag"] = "tag";
    DiscrError2["Mapping"] = "mapping";
  })(DiscrError || (exports.DiscrError = DiscrError = {}));
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/vocabularies/discriminator/index.js
var require_discriminator = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var codegen_1 = require_codegen();
  var types_1 = require_types();
  var compile_1 = require_compile();
  var ref_error_1 = require_ref_error();
  var util_1 = require_util();
  var error = {
    message: ({ params: { discrError, tagName } }) => discrError === types_1.DiscrError.Tag ? `tag "${tagName}" must be string` : `value of tag "${tagName}" must be in oneOf`,
    params: ({ params: { discrError, tag, tagName } }) => (0, codegen_1._)`{error: ${discrError}, tag: ${tagName}, tagValue: ${tag}}`
  };
  var def = {
    keyword: "discriminator",
    type: "object",
    schemaType: "object",
    error,
    code(cxt) {
      const { gen, data, schema, parentSchema, it } = cxt;
      const { oneOf } = parentSchema;
      if (!it.opts.discriminator) {
        throw new Error("discriminator: requires discriminator option");
      }
      const tagName = schema.propertyName;
      if (typeof tagName != "string")
        throw new Error("discriminator: requires propertyName");
      if (schema.mapping)
        throw new Error("discriminator: mapping is not supported");
      if (!oneOf)
        throw new Error("discriminator: requires oneOf keyword");
      const valid = gen.let("valid", false);
      const tag = gen.const("tag", (0, codegen_1._)`${data}${(0, codegen_1.getProperty)(tagName)}`);
      gen.if((0, codegen_1._)`typeof ${tag} == "string"`, () => validateMapping(), () => cxt.error(false, { discrError: types_1.DiscrError.Tag, tag, tagName }));
      cxt.ok(valid);
      function validateMapping() {
        const mapping = getMapping();
        gen.if(false);
        for (const tagValue in mapping) {
          gen.elseIf((0, codegen_1._)`${tag} === ${tagValue}`);
          gen.assign(valid, applyTagSchema(mapping[tagValue]));
        }
        gen.else();
        cxt.error(false, { discrError: types_1.DiscrError.Mapping, tag, tagName });
        gen.endIf();
      }
      function applyTagSchema(schemaProp) {
        const _valid = gen.name("valid");
        const schCxt = cxt.subschema({ keyword: "oneOf", schemaProp }, _valid);
        cxt.mergeEvaluated(schCxt, codegen_1.Name);
        return _valid;
      }
      function getMapping() {
        var _a;
        const oneOfMapping = {};
        const topRequired = hasRequired(parentSchema);
        let tagRequired = true;
        for (let i = 0;i < oneOf.length; i++) {
          let sch = oneOf[i];
          if ((sch === null || sch === undefined ? undefined : sch.$ref) && !(0, util_1.schemaHasRulesButRef)(sch, it.self.RULES)) {
            const ref = sch.$ref;
            sch = compile_1.resolveRef.call(it.self, it.schemaEnv.root, it.baseId, ref);
            if (sch instanceof compile_1.SchemaEnv)
              sch = sch.schema;
            if (sch === undefined)
              throw new ref_error_1.default(it.opts.uriResolver, it.baseId, ref);
          }
          const propSch = (_a = sch === null || sch === undefined ? undefined : sch.properties) === null || _a === undefined ? undefined : _a[tagName];
          if (typeof propSch != "object") {
            throw new Error(`discriminator: oneOf subschemas (or referenced schemas) must have "properties/${tagName}"`);
          }
          tagRequired = tagRequired && (topRequired || hasRequired(sch));
          addMappings(propSch, i);
        }
        if (!tagRequired)
          throw new Error(`discriminator: "${tagName}" must be required`);
        return oneOfMapping;
        function hasRequired({ required }) {
          return Array.isArray(required) && required.includes(tagName);
        }
        function addMappings(sch, i) {
          if (sch.const) {
            addMapping(sch.const, i);
          } else if (sch.enum) {
            for (const tagValue of sch.enum) {
              addMapping(tagValue, i);
            }
          } else {
            throw new Error(`discriminator: "properties/${tagName}" must have "const" or "enum"`);
          }
        }
        function addMapping(tagValue, i) {
          if (typeof tagValue != "string" || tagValue in oneOfMapping) {
            throw new Error(`discriminator: "${tagName}" values must be unique strings`);
          }
          oneOfMapping[tagValue] = i;
        }
      }
    }
  };
  exports.default = def;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/refs/json-schema-2020-12/schema.json
var require_schema = __commonJS(function(exports, module) {
  module.exports = {
    $schema: "https://json-schema.org/draft/2020-12/schema",
    $id: "https://json-schema.org/draft/2020-12/schema",
    $vocabulary: {
      "https://json-schema.org/draft/2020-12/vocab/core": true,
      "https://json-schema.org/draft/2020-12/vocab/applicator": true,
      "https://json-schema.org/draft/2020-12/vocab/unevaluated": true,
      "https://json-schema.org/draft/2020-12/vocab/validation": true,
      "https://json-schema.org/draft/2020-12/vocab/meta-data": true,
      "https://json-schema.org/draft/2020-12/vocab/format-annotation": true,
      "https://json-schema.org/draft/2020-12/vocab/content": true
    },
    $dynamicAnchor: "meta",
    title: "Core and Validation specifications meta-schema",
    allOf: [
      { $ref: "meta/core" },
      { $ref: "meta/applicator" },
      { $ref: "meta/unevaluated" },
      { $ref: "meta/validation" },
      { $ref: "meta/meta-data" },
      { $ref: "meta/format-annotation" },
      { $ref: "meta/content" }
    ],
    type: ["object", "boolean"],
    $comment: "This meta-schema also defines keywords that have appeared in previous drafts in order to prevent incompatible extensions as they remain in common use.",
    properties: {
      definitions: {
        $comment: '"definitions" has been replaced by "$defs".',
        type: "object",
        additionalProperties: { $dynamicRef: "#meta" },
        deprecated: true,
        default: {}
      },
      dependencies: {
        $comment: '"dependencies" has been split and replaced by "dependentSchemas" and "dependentRequired" in order to serve their differing semantics.',
        type: "object",
        additionalProperties: {
          anyOf: [{ $dynamicRef: "#meta" }, { $ref: "meta/validation#/$defs/stringArray" }]
        },
        deprecated: true,
        default: {}
      },
      $recursiveAnchor: {
        $comment: '"$recursiveAnchor" has been replaced by "$dynamicAnchor".',
        $ref: "meta/core#/$defs/anchorString",
        deprecated: true
      },
      $recursiveRef: {
        $comment: '"$recursiveRef" has been replaced by "$dynamicRef".',
        $ref: "meta/core#/$defs/uriReferenceString",
        deprecated: true
      }
    }
  };
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/refs/json-schema-2020-12/meta/applicator.json
var require_applicator2 = __commonJS(function(exports, module) {
  module.exports = {
    $schema: "https://json-schema.org/draft/2020-12/schema",
    $id: "https://json-schema.org/draft/2020-12/meta/applicator",
    $vocabulary: {
      "https://json-schema.org/draft/2020-12/vocab/applicator": true
    },
    $dynamicAnchor: "meta",
    title: "Applicator vocabulary meta-schema",
    type: ["object", "boolean"],
    properties: {
      prefixItems: { $ref: "#/$defs/schemaArray" },
      items: { $dynamicRef: "#meta" },
      contains: { $dynamicRef: "#meta" },
      additionalProperties: { $dynamicRef: "#meta" },
      properties: {
        type: "object",
        additionalProperties: { $dynamicRef: "#meta" },
        default: {}
      },
      patternProperties: {
        type: "object",
        additionalProperties: { $dynamicRef: "#meta" },
        propertyNames: { format: "regex" },
        default: {}
      },
      dependentSchemas: {
        type: "object",
        additionalProperties: { $dynamicRef: "#meta" },
        default: {}
      },
      propertyNames: { $dynamicRef: "#meta" },
      if: { $dynamicRef: "#meta" },
      then: { $dynamicRef: "#meta" },
      else: { $dynamicRef: "#meta" },
      allOf: { $ref: "#/$defs/schemaArray" },
      anyOf: { $ref: "#/$defs/schemaArray" },
      oneOf: { $ref: "#/$defs/schemaArray" },
      not: { $dynamicRef: "#meta" }
    },
    $defs: {
      schemaArray: {
        type: "array",
        minItems: 1,
        items: { $dynamicRef: "#meta" }
      }
    }
  };
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/refs/json-schema-2020-12/meta/unevaluated.json
var require_unevaluated2 = __commonJS(function(exports, module) {
  module.exports = {
    $schema: "https://json-schema.org/draft/2020-12/schema",
    $id: "https://json-schema.org/draft/2020-12/meta/unevaluated",
    $vocabulary: {
      "https://json-schema.org/draft/2020-12/vocab/unevaluated": true
    },
    $dynamicAnchor: "meta",
    title: "Unevaluated applicator vocabulary meta-schema",
    type: ["object", "boolean"],
    properties: {
      unevaluatedItems: { $dynamicRef: "#meta" },
      unevaluatedProperties: { $dynamicRef: "#meta" }
    }
  };
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/refs/json-schema-2020-12/meta/content.json
var require_content = __commonJS(function(exports, module) {
  module.exports = {
    $schema: "https://json-schema.org/draft/2020-12/schema",
    $id: "https://json-schema.org/draft/2020-12/meta/content",
    $vocabulary: {
      "https://json-schema.org/draft/2020-12/vocab/content": true
    },
    $dynamicAnchor: "meta",
    title: "Content vocabulary meta-schema",
    type: ["object", "boolean"],
    properties: {
      contentEncoding: { type: "string" },
      contentMediaType: { type: "string" },
      contentSchema: { $dynamicRef: "#meta" }
    }
  };
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/refs/json-schema-2020-12/meta/core.json
var require_core3 = __commonJS(function(exports, module) {
  module.exports = {
    $schema: "https://json-schema.org/draft/2020-12/schema",
    $id: "https://json-schema.org/draft/2020-12/meta/core",
    $vocabulary: {
      "https://json-schema.org/draft/2020-12/vocab/core": true
    },
    $dynamicAnchor: "meta",
    title: "Core vocabulary meta-schema",
    type: ["object", "boolean"],
    properties: {
      $id: {
        $ref: "#/$defs/uriReferenceString",
        $comment: "Non-empty fragments not allowed.",
        pattern: "^[^#]*#?$"
      },
      $schema: { $ref: "#/$defs/uriString" },
      $ref: { $ref: "#/$defs/uriReferenceString" },
      $anchor: { $ref: "#/$defs/anchorString" },
      $dynamicRef: { $ref: "#/$defs/uriReferenceString" },
      $dynamicAnchor: { $ref: "#/$defs/anchorString" },
      $vocabulary: {
        type: "object",
        propertyNames: { $ref: "#/$defs/uriString" },
        additionalProperties: {
          type: "boolean"
        }
      },
      $comment: {
        type: "string"
      },
      $defs: {
        type: "object",
        additionalProperties: { $dynamicRef: "#meta" }
      }
    },
    $defs: {
      anchorString: {
        type: "string",
        pattern: "^[A-Za-z_][-A-Za-z0-9._]*$"
      },
      uriString: {
        type: "string",
        format: "uri"
      },
      uriReferenceString: {
        type: "string",
        format: "uri-reference"
      }
    }
  };
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/refs/json-schema-2020-12/meta/format-annotation.json
var require_format_annotation = __commonJS(function(exports, module) {
  module.exports = {
    $schema: "https://json-schema.org/draft/2020-12/schema",
    $id: "https://json-schema.org/draft/2020-12/meta/format-annotation",
    $vocabulary: {
      "https://json-schema.org/draft/2020-12/vocab/format-annotation": true
    },
    $dynamicAnchor: "meta",
    title: "Format vocabulary meta-schema for annotation results",
    type: ["object", "boolean"],
    properties: {
      format: { type: "string" }
    }
  };
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/refs/json-schema-2020-12/meta/meta-data.json
var require_meta_data = __commonJS(function(exports, module) {
  module.exports = {
    $schema: "https://json-schema.org/draft/2020-12/schema",
    $id: "https://json-schema.org/draft/2020-12/meta/meta-data",
    $vocabulary: {
      "https://json-schema.org/draft/2020-12/vocab/meta-data": true
    },
    $dynamicAnchor: "meta",
    title: "Meta-data vocabulary meta-schema",
    type: ["object", "boolean"],
    properties: {
      title: {
        type: "string"
      },
      description: {
        type: "string"
      },
      default: true,
      deprecated: {
        type: "boolean",
        default: false
      },
      readOnly: {
        type: "boolean",
        default: false
      },
      writeOnly: {
        type: "boolean",
        default: false
      },
      examples: {
        type: "array",
        items: true
      }
    }
  };
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/refs/json-schema-2020-12/meta/validation.json
var require_validation2 = __commonJS(function(exports, module) {
  module.exports = {
    $schema: "https://json-schema.org/draft/2020-12/schema",
    $id: "https://json-schema.org/draft/2020-12/meta/validation",
    $vocabulary: {
      "https://json-schema.org/draft/2020-12/vocab/validation": true
    },
    $dynamicAnchor: "meta",
    title: "Validation vocabulary meta-schema",
    type: ["object", "boolean"],
    properties: {
      type: {
        anyOf: [
          { $ref: "#/$defs/simpleTypes" },
          {
            type: "array",
            items: { $ref: "#/$defs/simpleTypes" },
            minItems: 1,
            uniqueItems: true
          }
        ]
      },
      const: true,
      enum: {
        type: "array",
        items: true
      },
      multipleOf: {
        type: "number",
        exclusiveMinimum: 0
      },
      maximum: {
        type: "number"
      },
      exclusiveMaximum: {
        type: "number"
      },
      minimum: {
        type: "number"
      },
      exclusiveMinimum: {
        type: "number"
      },
      maxLength: { $ref: "#/$defs/nonNegativeInteger" },
      minLength: { $ref: "#/$defs/nonNegativeIntegerDefault0" },
      pattern: {
        type: "string",
        format: "regex"
      },
      maxItems: { $ref: "#/$defs/nonNegativeInteger" },
      minItems: { $ref: "#/$defs/nonNegativeIntegerDefault0" },
      uniqueItems: {
        type: "boolean",
        default: false
      },
      maxContains: { $ref: "#/$defs/nonNegativeInteger" },
      minContains: {
        $ref: "#/$defs/nonNegativeInteger",
        default: 1
      },
      maxProperties: { $ref: "#/$defs/nonNegativeInteger" },
      minProperties: { $ref: "#/$defs/nonNegativeIntegerDefault0" },
      required: { $ref: "#/$defs/stringArray" },
      dependentRequired: {
        type: "object",
        additionalProperties: {
          $ref: "#/$defs/stringArray"
        }
      }
    },
    $defs: {
      nonNegativeInteger: {
        type: "integer",
        minimum: 0
      },
      nonNegativeIntegerDefault0: {
        $ref: "#/$defs/nonNegativeInteger",
        default: 0
      },
      simpleTypes: {
        enum: ["array", "boolean", "integer", "null", "number", "object", "string"]
      },
      stringArray: {
        type: "array",
        items: { type: "string" },
        uniqueItems: true,
        default: []
      }
    }
  };
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/refs/json-schema-2020-12/index.js
var require_json_schema_2020_12 = __commonJS(function(exports) {
  Object.defineProperty(exports, "__esModule", { value: true });
  var metaSchema = require_schema();
  var applicator = require_applicator2();
  var unevaluated = require_unevaluated2();
  var content = require_content();
  var core = require_core3();
  var format = require_format_annotation();
  var metadata = require_meta_data();
  var validation = require_validation2();
  var META_SUPPORT_DATA = ["/properties"];
  function addMetaSchema2020($data) {
    [
      metaSchema,
      applicator,
      unevaluated,
      content,
      core,
      with$data(this, format),
      metadata,
      with$data(this, validation)
    ].forEach((sch) => this.addMetaSchema(sch, undefined, false));
    return this;
    function with$data(ajv, sch) {
      return $data ? ajv.$dataMetaSchema(sch, META_SUPPORT_DATA) : sch;
    }
  }
  exports.default = addMetaSchema2020;
});

// ../../node_modules/.bun/ajv@8.17.1/node_modules/ajv/dist/2020.js
var require__2020 = __commonJS(function(exports, module) {
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.MissingRefError = exports.ValidationError = exports.CodeGen = exports.Name = exports.nil = exports.stringify = exports.str = exports._ = exports.KeywordCxt = exports.Ajv2020 = undefined;
  var core_1 = require_core();
  var draft2020_1 = require_draft2020();
  var discriminator_1 = require_discriminator();
  var json_schema_2020_12_1 = require_json_schema_2020_12();
  var META_SCHEMA_ID = "https://json-schema.org/draft/2020-12/schema";

  class Ajv2020 extends core_1.default {
    constructor(opts = {}) {
      super({
        ...opts,
        dynamicRef: true,
        next: true,
        unevaluated: true
      });
    }
    _addVocabularies() {
      super._addVocabularies();
      draft2020_1.default.forEach((v) => this.addVocabulary(v));
      if (this.opts.discriminator)
        this.addKeyword(discriminator_1.default);
    }
    _addDefaultMetaSchema() {
      super._addDefaultMetaSchema();
      const { $data, meta } = this.opts;
      if (!meta)
        return;
      json_schema_2020_12_1.default.call(this, $data);
      this.refs["http://json-schema.org/schema"] = META_SCHEMA_ID;
    }
    defaultMeta() {
      return this.opts.defaultMeta = super.defaultMeta() || (this.getSchema(META_SCHEMA_ID) ? META_SCHEMA_ID : undefined);
    }
  }
  exports.Ajv2020 = Ajv2020;
  module.exports = exports = Ajv2020;
  module.exports.Ajv2020 = Ajv2020;
  Object.defineProperty(exports, "__esModule", { value: true });
  exports.default = Ajv2020;
  var validate_1 = require_validate();
  Object.defineProperty(exports, "KeywordCxt", { enumerable: true, get: function() {
    return validate_1.KeywordCxt;
  } });
  var codegen_1 = require_codegen();
  Object.defineProperty(exports, "_", { enumerable: true, get: function() {
    return codegen_1._;
  } });
  Object.defineProperty(exports, "str", { enumerable: true, get: function() {
    return codegen_1.str;
  } });
  Object.defineProperty(exports, "stringify", { enumerable: true, get: function() {
    return codegen_1.stringify;
  } });
  Object.defineProperty(exports, "nil", { enumerable: true, get: function() {
    return codegen_1.nil;
  } });
  Object.defineProperty(exports, "Name", { enumerable: true, get: function() {
    return codegen_1.Name;
  } });
  Object.defineProperty(exports, "CodeGen", { enumerable: true, get: function() {
    return codegen_1.CodeGen;
  } });
  var validation_error_1 = require_validation_error();
  Object.defineProperty(exports, "ValidationError", { enumerable: true, get: function() {
    return validation_error_1.default;
  } });
  var ref_error_1 = require_ref_error();
  Object.defineProperty(exports, "MissingRefError", { enumerable: true, get: function() {
    return ref_error_1.default;
  } });
});

// src/index.ts
import { TASK_SUBAGENT_EVENT_CHANNEL, TASK_SUBAGENT_LIFECYCLE_CHANNEL, TASK_SUBAGENT_PROGRESS_CHANNEL } from "@oh-my-pi/pi-coding-agent/task";

// src/context.ts
import { homedir } from "os";

// src/platform.ts
import { existsSync as existsSync2, readFileSync as readFileSync2 } from "fs";
import { join as join2 } from "path";

// src/runtime.ts
import { spawn } from "child_process";
import { createHash } from "crypto";
import { existsSync, lstatSync, realpathSync, statSync } from "fs";
import { dirname, isAbsolute, join } from "path";

// src/client.ts
import { createConnection } from "net";

// ../protocol-ts/src/validate.ts
var import__2020 = __toESM(require__2020(), 1);
// ../protocol-ts/schema/batman.schema.json
var batman_schema_default = {
  $schema: "https://json-schema.org/draft/2020-12/schema",
  title: "ProtocolDocument",
  description: "Root schema document referencing every exported request/result/event\ntype, so that a single `schemars` invocation produces one JSON Schema\nwith everything reachable from the wire protocol in `$defs`.",
  type: "object",
  properties: {
    initializeParams: {
      $ref: "#/$defs/InitializeParams"
    },
    initializeResult: {
      $ref: "#/$defs/InitializeResult"
    },
    eventEnvelope: {
      $ref: "#/$defs/EventEnvelope"
    },
    runtimeEvent: {
      $ref: "#/$defs/RuntimeEvent"
    },
    displayBackend: {
      $ref: "#/$defs/DisplayBackend"
    },
    displayConfig: {
      $ref: "#/$defs/DisplayConfig"
    },
    displayStatus: {
      $ref: "#/$defs/DisplayStatus"
    },
    jsonRpcRequest: {
      $ref: "#/$defs/JsonRpcRequest"
    },
    jsonRpcResponse: {
      $ref: "#/$defs/JsonRpcResponse"
    },
    jsonRpcErrorResponse: {
      $ref: "#/$defs/JsonRpcErrorResponse"
    },
    jsonRpcNotification: {
      $ref: "#/$defs/JsonRpcNotification"
    },
    runtimeStatus: {
      $ref: "#/$defs/RuntimeStatus"
    },
    artifactListResult: {
      $ref: "#/$defs/ArtifactListResult"
    },
    artifactFetchResult: {
      $ref: "#/$defs/ArtifactFetchResult"
    },
    inspectResult: {
      $ref: "#/$defs/InspectResult"
    },
    applyResult: {
      $ref: "#/$defs/ApplyResult"
    },
    workspaceInfo: {
      $ref: "#/$defs/WorkspaceInfo"
    },
    policyViolationListResult: {
      $ref: "#/$defs/PolicyViolationListResult"
    },
    runResultResult: {
      description: "`run/result` result payload.",
      $ref: "#/$defs/RunResultResult"
    }
  },
  required: [
    "initializeParams",
    "initializeResult",
    "eventEnvelope",
    "runtimeEvent",
    "displayBackend",
    "displayConfig",
    "displayStatus",
    "jsonRpcRequest",
    "jsonRpcResponse",
    "jsonRpcErrorResponse",
    "jsonRpcNotification",
    "runtimeStatus",
    "artifactListResult",
    "artifactFetchResult",
    "inspectResult",
    "applyResult",
    "workspaceInfo",
    "policyViolationListResult",
    "runResultResult"
  ],
  $defs: {
    InitializeParams: {
      description: "Parameters for the `initialize` request.",
      type: "object",
      properties: {
        client: {
          $ref: "#/$defs/ClientInfo"
        },
        supported: {
          $ref: "#/$defs/VersionRange"
        },
        repository: {
          $ref: "#/$defs/RepositoryIdentity"
        },
        auth: {
          $ref: "#/$defs/ClientAuth"
        },
        capabilities: {
          $ref: "#/$defs/ClientCapabilities"
        },
        lastSequence: {
          type: [
            "integer",
            "null"
          ],
          format: "uint64",
          minimum: 0
        }
      },
      additionalProperties: false,
      required: [
        "client",
        "supported",
        "repository",
        "auth",
        "capabilities"
      ]
    },
    ClientInfo: {
      description: `Identifies the connecting client implementation (name + version), for
diagnostics only.`,
      type: "object",
      properties: {
        name: {
          type: "string"
        },
        version: {
          type: "string"
        }
      },
      additionalProperties: false,
      required: [
        "name",
        "version"
      ]
    },
    VersionRange: {
      description: "An inclusive range of protocol versions a client (or runtime) supports.",
      type: "object",
      properties: {
        min: {
          $ref: "#/$defs/ProtocolVersion"
        },
        max: {
          $ref: "#/$defs/ProtocolVersion"
        }
      },
      additionalProperties: false,
      required: [
        "min",
        "max"
      ]
    },
    ProtocolVersion: {
      description: "A BATMAN protocol version, expressed as `major.minor` with no patch\ncomponent (patch-level changes must be backward compatible).",
      type: "object",
      properties: {
        major: {
          type: "integer",
          format: "uint16",
          minimum: 0,
          maximum: 65535
        },
        minor: {
          type: "integer",
          format: "uint16",
          minimum: 0,
          maximum: 65535
        }
      },
      additionalProperties: false,
      required: [
        "major",
        "minor"
      ]
    },
    RepositoryIdentity: {
      description: `Identifies a repository on disk, independent of any particular runtime
instance.`,
      type: "object",
      properties: {
        canonicalPath: {
          type: "string"
        },
        vcsRoot: {
          type: "string"
        }
      },
      additionalProperties: false,
      required: [
        "canonicalPath",
        "vcsRoot"
      ]
    },
    ClientAuth: {
      description: "Authentication payload presented by a connecting client. The `role` tag\ndetermines which shape the remaining fields take.",
      oneOf: [
        {
          type: "object",
          properties: {
            instanceId: {
              type: "string"
            },
            agentDirectory: {
              type: "string"
            },
            role: {
              type: "string",
              const: "ompExtension"
            }
          },
          additionalProperties: false,
          required: [
            "role",
            "instanceId",
            "agentDirectory"
          ]
        },
        {
          type: "object",
          properties: {
            instanceId: {
              type: "string"
            },
            scopeToken: {
              type: "string"
            },
            role: {
              type: "string",
              const: "workerMcp"
            }
          },
          additionalProperties: false,
          required: [
            "role",
            "instanceId",
            "scopeToken"
          ]
        },
        {
          type: "object",
          properties: {
            instanceId: {
              type: "string"
            },
            role: {
              type: "string",
              const: "display"
            }
          },
          additionalProperties: false,
          required: [
            "role",
            "instanceId"
          ]
        }
      ]
    },
    ClientCapabilities: {
      description: "Capabilities a connecting client declares support for.",
      type: "object",
      properties: {
        eventReplay: {
          type: "boolean"
        },
        maxFrameBytes: {
          type: "integer",
          format: "uint32",
          minimum: 0
        }
      },
      additionalProperties: false,
      required: [
        "eventReplay",
        "maxFrameBytes"
      ]
    },
    InitializeResult: {
      description: "Result of a successful `initialize` request.",
      type: "object",
      properties: {
        runtime: {
          $ref: "#/$defs/RuntimeInfo"
        },
        negotiated: {
          $ref: "#/$defs/ProtocolVersion"
        },
        projectId: {
          $ref: "#/$defs/ProjectId"
        },
        principal: {
          $ref: "#/$defs/ClientPrincipalSummary"
        },
        allowedMethods: {
          type: "array",
          items: {
            $ref: "#/$defs/BatmanMethod"
          }
        },
        capabilities: {
          $ref: "#/$defs/RuntimeCapabilities"
        },
        nextSequence: {
          type: "integer",
          format: "uint64",
          minimum: 0
        }
      },
      additionalProperties: false,
      required: [
        "runtime",
        "negotiated",
        "projectId",
        "principal",
        "allowedMethods",
        "capabilities",
        "nextSequence"
      ]
    },
    RuntimeInfo: {
      description: `Identifies the runtime implementation (name + version), for diagnostics
only.`,
      type: "object",
      properties: {
        name: {
          type: "string"
        },
        version: {
          type: "string"
        }
      },
      additionalProperties: false,
      required: [
        "name",
        "version"
      ]
    },
    ProjectId: {
      description: "Identifies a repository/project managed by the BATMAN runtime.",
      type: "string"
    },
    ClientPrincipalSummary: {
      description: `A summary of the authenticated client, echoed back so the client can
confirm how the runtime identified it.`,
      type: "object",
      properties: {
        role: {
          $ref: "#/$defs/ClientRole"
        },
        instanceId: {
          type: "string"
        },
        scopedRunId: {
          description: "The run this connection is scoped to. `None` for every role except\n`workerMcp`, whose scope-token binding determines it -- never a\nvalue the client can request or override.",
          anyOf: [
            {
              $ref: "#/$defs/RunId"
            },
            {
              type: "null"
            }
          ]
        },
        scopedTaskId: {
          description: "The task this connection is scoped to, alongside `scopedRunId`.",
          anyOf: [
            {
              $ref: "#/$defs/TaskId"
            },
            {
              type: "null"
            }
          ]
        },
        scopedWorkerId: {
          description: "The worker this connection is scoped to, alongside `scopedRunId`.\nA `workerMcp` client uses this (never a self-declared value) as\nthe authoritative sender identity for `coordination/send`.",
          anyOf: [
            {
              $ref: "#/$defs/WorkerId"
            },
            {
              type: "null"
            }
          ]
        }
      },
      additionalProperties: false,
      required: [
        "role",
        "instanceId"
      ]
    },
    ClientRole: {
      description: "The role a connecting client authenticates as.",
      type: "string",
      enum: [
        "ompExtension",
        "workerMcp",
        "display"
      ]
    },
    RunId: {
      description: "Identifies a single run of a task.",
      type: "string"
    },
    TaskId: {
      description: "Identifies a task tracked by the runtime.",
      type: "string"
    },
    WorkerId: {
      description: "Identifies a worker process spawned by the runtime.",
      type: "string"
    },
    BatmanMethod: {
      description: `All JSON-RPC methods implemented by the BATMAN runtime, including
orchestration extension methods.

Serialized as the literal method name string used on the wire.`,
      oneOf: [
        {
          type: "string",
          enum: [
            "initialize",
            "runtime/status",
            "events/subscribe",
            "events/replay",
            "task/upsert",
            "task/get",
            "worker/create",
            "worker/list",
            "worker/get",
            "run/submit",
            "run/list",
            "run/get",
            "run/retry",
            "run/cancel",
            "run/result",
            "message/send",
            "message/list",
            "approval/list",
            "approval/decide",
            "coordination/child/list",
            "coordination/child/decide",
            "coordination/task",
            "coordination/peers",
            "coordination/send",
            "coordination/requestChild",
            "coordination/publishArtifact",
            "coordination/reportBlocked",
            "coordination/askPolicy",
            "coordination/peerWorkspace",
            "coordination/artifactList",
            "coordination/artifactFetch",
            "reconcile/omp",
            "profile/register",
            "workspace/acquire",
            "workspace/get",
            "workspace/release",
            "workspace/inspect",
            "workspace/apply",
            "artifact/list",
            "artifact/fetch",
            "policy/violation/decide"
          ]
        },
        {
          description: "Gracefully stops the daemon. Arbitrated (R82): refused with\n`-32602` while any run is live or another connection is being\nserved, unless `params.force == true` (the deliberate, logged\noperator escape hatch). The out-of-band `crewd stop`/SIGTERM\npath is deliberately unarbitrated.",
          type: "string",
          const: "runtime/shutdown"
        },
        {
          description: `Lists a project's recorded policy violations with their decision
state, so an operator can find which violation still holds a
quarantine without diffing the raw event stream (R80).`,
          type: "string",
          const: "policy/violation/list"
        }
      ]
    },
    RuntimeCapabilities: {
      description: "Capabilities the runtime grants for the negotiated session.",
      type: "object",
      properties: {
        maxFrameBytes: {
          description: "The negotiated maximum NDJSON frame size, in bytes.",
          type: "integer",
          format: "uint32",
          minimum: 0
        },
        peerCredentialsVerified: {
          description: "Whether the runtime was able to verify the peer's OS credentials for\nthis connection. `false` on platforms where peer credential lookup is\nunavailable.",
          type: "boolean"
        }
      },
      additionalProperties: false,
      required: [
        "maxFrameBytes",
        "peerCredentialsVerified"
      ]
    },
    EventEnvelope: {
      description: `The envelope wrapping every durable runtime event, carrying its sequence
number and routing metadata.`,
      type: "object",
      properties: {
        sequence: {
          type: "integer",
          format: "uint64",
          minimum: 0
        },
        timestamp: {
          $ref: "#/$defs/Timestamp"
        },
        projectId: {
          $ref: "#/$defs/ProjectId"
        },
        taskId: {
          anyOf: [
            {
              $ref: "#/$defs/TaskId"
            },
            {
              type: "null"
            }
          ]
        },
        workerId: {
          anyOf: [
            {
              $ref: "#/$defs/WorkerId"
            },
            {
              type: "null"
            }
          ]
        },
        runId: {
          anyOf: [
            {
              $ref: "#/$defs/RunId"
            },
            {
              type: "null"
            }
          ]
        },
        parentWorkerId: {
          anyOf: [
            {
              $ref: "#/$defs/WorkerId"
            },
            {
              type: "null"
            }
          ]
        },
        source: {
          $ref: "#/$defs/EventSource"
        },
        event: {
          $ref: "#/$defs/RuntimeEvent"
        },
        vendorEventRef: {
          type: [
            "string",
            "null"
          ]
        }
      },
      additionalProperties: false,
      required: [
        "sequence",
        "timestamp",
        "projectId",
        "source",
        "event"
      ]
    },
    Timestamp: {
      description: `Canonical UTC RFC 3339 timestamp text, as carried on the wire.

Rather than expose [\`time::OffsetDateTime\`] across generated bindings,
BATMAN normalizes every timestamp to a UTC RFC 3339 string at
construction time; downstream consumers (including schemars/ts-rs) only
ever see a plain string.`,
      type: "string"
    },
    EventSource: {
      description: "Identifies which subsystem produced an event.",
      type: "string",
      enum: [
        "runtime"
      ]
    },
    RuntimeEvent: {
      description: "A sanitized, durable runtime event. Fields are plain, already-sanitized\ntypes (never [`Classified`]) so that raw thinking/secret content can\nnever reach the durable log through this type.",
      oneOf: [
        {
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "runtimeStarted"
            }
          },
          required: [
            "type"
          ],
          additionalProperties: false
        },
        {
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "runtimeStopping"
            }
          },
          required: [
            "type"
          ],
          additionalProperties: false
        },
        {
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "diagnostic"
            },
            payload: {
              type: "object",
              properties: {
                level: {
                  $ref: "#/$defs/DiagnosticLevel"
                },
                code: {
                  type: "string"
                },
                message: {
                  type: "string"
                }
              },
              additionalProperties: false,
              required: [
                "level",
                "code",
                "message"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          description: "A task was created or updated via `task/upsert`.",
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "taskEvent"
            },
            payload: {
              type: "object",
              properties: {
                kind: {
                  $ref: "#/$defs/RuntimeEventKind"
                },
                taskId: {
                  $ref: "#/$defs/TaskId"
                },
                ownerClientInstanceId: {
                  type: "string"
                },
                revision: {
                  type: "integer",
                  format: "uint64",
                  minimum: 0
                }
              },
              additionalProperties: false,
              required: [
                "kind",
                "taskId",
                "ownerClientInstanceId",
                "revision"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          description: "A worker was created via `worker/create`.",
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "workerEvent"
            },
            payload: {
              type: "object",
              properties: {
                kind: {
                  $ref: "#/$defs/RuntimeEventKind"
                },
                workerId: {
                  $ref: "#/$defs/WorkerId"
                },
                profileId: {
                  type: "string"
                }
              },
              additionalProperties: false,
              required: [
                "kind",
                "workerId",
                "profileId"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          description: "A run entered a new lifecycle state.",
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "runEvent"
            },
            payload: {
              type: "object",
              properties: {
                kind: {
                  $ref: "#/$defs/RuntimeEventKind"
                },
                runId: {
                  $ref: "#/$defs/RunId"
                },
                taskId: {
                  $ref: "#/$defs/TaskId"
                },
                workerId: {
                  $ref: "#/$defs/WorkerId"
                },
                state: {
                  type: "string"
                }
              },
              additionalProperties: false,
              required: [
                "kind",
                "runId",
                "taskId",
                "workerId",
                "state"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          description: "Flags on a run were changed.",
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "runFlagsEvent"
            },
            payload: {
              type: "object",
              properties: {
                runId: {
                  $ref: "#/$defs/RunId"
                },
                flags: {
                  $ref: "#/$defs/RunFlags"
                }
              },
              additionalProperties: false,
              required: [
                "runId",
                "flags"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          description: "A message was recorded, sent, acknowledged, or failed.",
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "messageEvent"
            },
            payload: {
              type: "object",
              properties: {
                kind: {
                  $ref: "#/$defs/RuntimeEventKind"
                },
                messageId: {
                  $ref: "#/$defs/MessageId"
                },
                runId: {
                  $ref: "#/$defs/RunId"
                },
                taskId: {
                  $ref: "#/$defs/TaskId"
                },
                deliveryState: {
                  type: "string"
                }
              },
              additionalProperties: false,
              required: [
                "kind",
                "messageId",
                "runId",
                "taskId",
                "deliveryState"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          description: "An approval request was created.",
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "approvalEvent"
            },
            payload: {
              type: "object",
              properties: {
                kind: {
                  $ref: "#/$defs/RuntimeEventKind"
                },
                approvalId: {
                  $ref: "#/$defs/ApprovalId"
                },
                runId: {
                  $ref: "#/$defs/RunId"
                },
                taskId: {
                  $ref: "#/$defs/TaskId"
                },
                action: {
                  type: "string"
                },
                decidedBy: {
                  anyOf: [
                    {
                      $ref: "#/$defs/DecidedBy"
                    },
                    {
                      type: "null"
                    }
                  ]
                },
                reason: {
                  description: "The decision's rationale; present only on `ApprovalDecided`\nevents written after R59. Optional in both directions so\nevents persisted before the field existed still deserialize.",
                  type: [
                    "string",
                    "null"
                  ]
                }
              },
              additionalProperties: false,
              required: [
                "kind",
                "approvalId",
                "runId",
                "taskId",
                "action"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          description: "A child worker was requested or denied.",
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "childEvent"
            },
            payload: {
              type: "object",
              properties: {
                kind: {
                  $ref: "#/$defs/RuntimeEventKind"
                },
                parentRunId: {
                  $ref: "#/$defs/RunId"
                },
                childTaskId: {
                  anyOf: [
                    {
                      $ref: "#/$defs/TaskId"
                    },
                    {
                      type: "null"
                    }
                  ]
                },
                childWorkerId: {
                  anyOf: [
                    {
                      $ref: "#/$defs/WorkerId"
                    },
                    {
                      type: "null"
                    }
                  ]
                },
                childRunId: {
                  anyOf: [
                    {
                      $ref: "#/$defs/RunId"
                    },
                    {
                      type: "null"
                    }
                  ]
                },
                reason: {
                  type: [
                    "string",
                    "null"
                  ]
                }
              },
              additionalProperties: false,
              required: [
                "kind",
                "parentRunId"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          description: "Ownership of a task was rebound via `reconcile/omp`.",
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "reconcileEvent"
            },
            payload: {
              type: "object",
              properties: {
                taskId: {
                  $ref: "#/$defs/TaskId"
                },
                oldOwnerClientInstanceId: {
                  type: "string"
                },
                newOwnerClientInstanceId: {
                  type: "string"
                },
                revision: {
                  type: "integer",
                  format: "uint64",
                  minimum: 0
                }
              },
              additionalProperties: false,
              required: [
                "taskId",
                "oldOwnerClientInstanceId",
                "newOwnerClientInstanceId",
                "revision"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          description: "A supervised adapter process started or exited.",
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "adapterProcessEvent"
            },
            payload: {
              type: "object",
              properties: {
                kind: {
                  $ref: "#/$defs/RuntimeEventKind"
                },
                runId: {
                  $ref: "#/$defs/RunId"
                },
                taskId: {
                  $ref: "#/$defs/TaskId"
                },
                workerId: {
                  $ref: "#/$defs/WorkerId"
                },
                pid: {
                  type: [
                    "integer",
                    "null"
                  ],
                  format: "uint32",
                  minimum: 0
                },
                exitCode: {
                  type: [
                    "integer",
                    "null"
                  ],
                  format: "int32"
                },
                signal: {
                  type: [
                    "string",
                    "null"
                  ]
                }
              },
              additionalProperties: false,
              required: [
                "kind",
                "runId",
                "taskId",
                "workerId"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          description: `A worker adapter established (or re-established) its vendor
session/thread identifier.`,
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "adapterVendorSessionEvent"
            },
            payload: {
              type: "object",
              properties: {
                runId: {
                  $ref: "#/$defs/RunId"
                },
                taskId: {
                  $ref: "#/$defs/TaskId"
                },
                workerId: {
                  $ref: "#/$defs/WorkerId"
                },
                vendorSessionId: {
                  type: "string"
                }
              },
              additionalProperties: false,
              required: [
                "runId",
                "taskId",
                "workerId",
                "vendorSessionId"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          description: "A visible message chunk or final message from a worker adapter.\n`text` has already crossed the redaction boundary; `None` means\nthe entire fragment was `Thinking`/`Secret`-classified and was\ndropped, not that the message was empty.",
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "adapterMessageEvent"
            },
            payload: {
              type: "object",
              properties: {
                kind: {
                  $ref: "#/$defs/RuntimeEventKind"
                },
                runId: {
                  $ref: "#/$defs/RunId"
                },
                taskId: {
                  $ref: "#/$defs/TaskId"
                },
                workerId: {
                  $ref: "#/$defs/WorkerId"
                },
                role: {
                  type: "string"
                },
                text: {
                  type: [
                    "string",
                    "null"
                  ]
                }
              },
              additionalProperties: false,
              required: [
                "kind",
                "runId",
                "taskId",
                "workerId",
                "role"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          description: "A tool call lifecycle event from a worker adapter. `detail` has\nalready crossed the redaction boundary; `None` means the detail\nfragment was `Thinking`/`Secret`-classified and was dropped, not\nthat it was empty.",
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "adapterToolEvent"
            },
            payload: {
              type: "object",
              properties: {
                kind: {
                  $ref: "#/$defs/RuntimeEventKind"
                },
                runId: {
                  $ref: "#/$defs/RunId"
                },
                taskId: {
                  $ref: "#/$defs/TaskId"
                },
                workerId: {
                  $ref: "#/$defs/WorkerId"
                },
                toolCallId: {
                  type: "string"
                },
                name: {
                  type: "string"
                },
                ok: {
                  type: [
                    "boolean",
                    "null"
                  ]
                },
                detail: {
                  type: [
                    "string",
                    "null"
                  ]
                }
              },
              additionalProperties: false,
              required: [
                "kind",
                "runId",
                "taskId",
                "workerId",
                "toolCallId",
                "name"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          description: "Usage/cost reported by a worker adapter.",
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "adapterUsageEvent"
            },
            payload: {
              type: "object",
              properties: {
                runId: {
                  $ref: "#/$defs/RunId"
                },
                taskId: {
                  $ref: "#/$defs/TaskId"
                },
                workerId: {
                  $ref: "#/$defs/WorkerId"
                },
                inputTokens: {
                  type: "integer",
                  format: "uint64",
                  minimum: 0
                },
                outputTokens: {
                  type: "integer",
                  format: "uint64",
                  minimum: 0
                },
                costUsd: {
                  type: [
                    "number",
                    "null"
                  ],
                  format: "double"
                }
              },
              additionalProperties: false,
              required: [
                "runId",
                "taskId",
                "workerId",
                "inputTokens",
                "outputTokens"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          description: "An artifact produced by a worker adapter.",
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "adapterArtifactEvent"
            },
            payload: {
              type: "object",
              properties: {
                runId: {
                  $ref: "#/$defs/RunId"
                },
                taskId: {
                  $ref: "#/$defs/TaskId"
                },
                workerId: {
                  $ref: "#/$defs/WorkerId"
                },
                artifactId: {
                  $ref: "#/$defs/ArtifactId"
                },
                artifactKind: {
                  type: "string"
                }
              },
              additionalProperties: false,
              required: [
                "runId",
                "taskId",
                "workerId",
                "artifactId",
                "artifactKind"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          description: "A worker adapter's protocol health changed. `detail` has already\ncrossed the redaction boundary; `None` means the detail fragment\nwas `Thinking`/`Secret`-classified and was dropped, not that it\nwas empty.",
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "adapterProtocolHealthEvent"
            },
            payload: {
              type: "object",
              properties: {
                runId: {
                  $ref: "#/$defs/RunId"
                },
                taskId: {
                  $ref: "#/$defs/TaskId"
                },
                workerId: {
                  $ref: "#/$defs/WorkerId"
                },
                healthy: {
                  type: "boolean"
                },
                detail: {
                  type: [
                    "string",
                    "null"
                  ]
                }
              },
              additionalProperties: false,
              required: [
                "runId",
                "taskId",
                "workerId",
                "healthy"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          description: "A workspace lease lifecycle event (lease acquire/release/inspect/apply/cleanup).",
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "workspaceEvent"
            },
            payload: {
              type: "object",
              properties: {
                kind: {
                  $ref: "#/$defs/WorkspaceEvent"
                },
                runId: {
                  $ref: "#/$defs/RunId"
                },
                leaseId: {
                  type: "string"
                }
              },
              additionalProperties: false,
              required: [
                "kind",
                "runId",
                "leaseId"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          description: `A worker adapter observed a vendor-created child worker, emitted
even when the adapter declares \`nested: none\` -- emission alone
never upgrades a declared capability; conformance/policy decide
what an unexpected observation means.`,
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "adapterNestedWorkerEvent"
            },
            payload: {
              type: "object",
              properties: {
                runId: {
                  $ref: "#/$defs/RunId"
                },
                taskId: {
                  $ref: "#/$defs/TaskId"
                },
                workerId: {
                  $ref: "#/$defs/WorkerId"
                },
                vendorChildId: {
                  type: "string"
                },
                vendorParentRef: {
                  type: "string"
                }
              },
              additionalProperties: false,
              required: [
                "runId",
                "taskId",
                "workerId",
                "vendorChildId",
                "vendorParentRef"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "policyViolationRecorded"
            },
            payload: {
              type: "object",
              properties: {
                kind: {
                  $ref: "#/$defs/RuntimeEventKind"
                },
                runId: {
                  $ref: "#/$defs/RunId"
                },
                taskId: {
                  $ref: "#/$defs/TaskId"
                },
                workerId: {
                  $ref: "#/$defs/WorkerId"
                }
              },
              additionalProperties: false,
              required: [
                "kind",
                "runId",
                "taskId",
                "workerId"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "policyViolationDecided"
            },
            payload: {
              type: "object",
              properties: {
                kind: {
                  $ref: "#/$defs/RuntimeEventKind"
                },
                runId: {
                  $ref: "#/$defs/RunId"
                },
                taskId: {
                  $ref: "#/$defs/TaskId"
                },
                workerId: {
                  $ref: "#/$defs/WorkerId"
                }
              },
              additionalProperties: false,
              required: [
                "kind",
                "runId",
                "taskId",
                "workerId"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          description: "A display backend attached or detached a Batman-owned pane.",
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "displayEvent"
            },
            payload: {
              type: "object",
              properties: {
                kind: {
                  $ref: "#/$defs/RuntimeEventKind"
                },
                runId: {
                  $ref: "#/$defs/RunId"
                },
                backend: {
                  $ref: "#/$defs/DisplayBackend"
                },
                placement: {
                  $ref: "#/$defs/DisplayPlacement"
                },
                paneRef: {
                  description: `The vendor-assigned pane identifier only -- never terminal
contents, never an absolute socket or filesystem path.`,
                  type: "string"
                }
              },
              additionalProperties: false,
              required: [
                "kind",
                "runId",
                "backend",
                "placement",
                "paneRef"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          description: "A human typed directly into a native pane, bypassing the\nadapter. Sets the run's `needsReconciliation` flag.",
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "outOfBandInput"
            },
            payload: {
              type: "object",
              properties: {
                runId: {
                  $ref: "#/$defs/RunId"
                },
                backend: {
                  $ref: "#/$defs/DisplayBackend"
                },
                paneRef: {
                  type: "string"
                }
              },
              additionalProperties: false,
              required: [
                "runId",
                "backend",
                "paneRef"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        }
      ]
    },
    DiagnosticLevel: {
      description: "The severity of a [`RuntimeEvent::Diagnostic`].",
      type: "string",
      enum: [
        "info",
        "warning",
        "error"
      ]
    },
    RuntimeEventKind: {
      description: `The semantic kind of an orchestration event stored in the durable journal.

Every record creation, lifecycle transition, flag change, message delivery
change, and approval request/decision produces one of these variants.`,
      oneOf: [
        {
          type: "string",
          enum: [
            "taskCreated",
            "taskUpdated",
            "workerCreated",
            "runQueued",
            "runStarting",
            "runWorking",
            "runWaitingUser",
            "runWaitingPeer",
            "runPaused",
            "runSucceeded",
            "runFailed",
            "runCancelled",
            "runLost",
            "runFlagsChanged",
            "messageRecorded",
            "messageSent",
            "messageAcknowledged",
            "messageFailed",
            "approvalRequested",
            "approvalDecided",
            "childWorkerRequested",
            "childWorkerRequestDenied",
            "reconcileOwnershipChanged"
          ]
        },
        {
          description: `OMP accepted a pending child-worker request, binding the created
child task/worker/run ids. Distinct from
[\`Self::ChildWorkerRequested\`] so a consumer never has to infer
"accepted" from whether the child ids happen to be populated
(R83). Additive and forward-safe; a pre-R83 binary replaying a
post-R83 journal fails on the unknown variant, the same
forward-only property as every event-kind addition.`,
          type: "string",
          const: "childWorkerAccepted"
        },
        {
          description: "A worker adapter's supervised process started.",
          type: "string",
          const: "adapterProcessStarted"
        },
        {
          description: "A worker adapter's supervised process exited.",
          type: "string",
          const: "adapterProcessExited"
        },
        {
          description: `A worker adapter established (or re-established) its vendor
session/thread identifier.`,
          type: "string",
          const: "adapterVendorSessionEstablished"
        },
        {
          description: "A worker adapter streamed a partial visible-message chunk.",
          type: "string",
          const: "adapterMessageChunk"
        },
        {
          description: "A worker adapter completed a visible message.",
          type: "string",
          const: "adapterMessageFinal"
        },
        {
          description: "A worker adapter's tool call started.",
          type: "string",
          const: "adapterToolStarted"
        },
        {
          description: "A worker adapter's tool call reported progress.",
          type: "string",
          const: "adapterToolProgress"
        },
        {
          description: "A worker adapter's tool call finished.",
          type: "string",
          const: "adapterToolResult"
        },
        {
          description: "A worker adapter reported usage/cost.",
          type: "string",
          const: "adapterUsageReported"
        },
        {
          description: "A worker adapter produced an artifact.",
          type: "string",
          const: "adapterArtifactProduced"
        },
        {
          description: "A worker adapter's protocol health changed.",
          type: "string",
          const: "adapterProtocolHealthChanged"
        },
        {
          description: "A worker adapter observed a vendor-created child, regardless of its\ndeclared `nested` capability.",
          type: "string",
          const: "adapterNestedWorkerObserved"
        },
        {
          description: "A display backend attached a Batman-owned pane to a run.",
          type: "string",
          const: "displayPaneAttached"
        },
        {
          description: "A display backend detached (closed) a Batman-owned pane.",
          type: "string",
          const: "displayPaneDetached"
        },
        {
          description: `A policy violation was recorded (model not allowed, concurrency
ceiling exceeded, nested worker denied, or adapter not authorized).`,
          type: "object",
          properties: {
            policyViolation: {
              type: "object",
              properties: {
                profile_id: {
                  type: "string"
                },
                adapter: {
                  type: "string"
                },
                model: {
                  type: "string"
                },
                violation_kind: {
                  type: "string"
                },
                reason: {
                  type: "string"
                },
                is_nested: {
                  type: "boolean"
                }
              },
              required: [
                "profile_id",
                "adapter",
                "model",
                "violation_kind",
                "reason",
                "is_nested"
              ]
            }
          },
          required: [
            "policyViolation"
          ],
          additionalProperties: false
        },
        {
          description: "A policy violation was recorded for an already-running worker (mid-run\nviolation, not pre-authorization). Quarantine/cancel state is tracked\nin `Run.flags.policy_quarantined`.",
          type: "object",
          properties: {
            policyViolationRecorded: {
              type: "object",
              properties: {
                violation_id: {
                  $ref: "#/$defs/PolicyViolationId"
                },
                code: {
                  description: "The machine-readable violation code: `nested_worker_denied` or\n`cost_ceiling_exceeded`. New codes are added here, never invented\nat a call site.",
                  type: "string"
                },
                observed_event_sequence: {
                  description: `The sequence of the event that triggered this violation, so an
operator can correlate the violation to its cause.`,
                  type: "integer",
                  format: "uint64",
                  minimum: 0
                },
                policy_fingerprint: {
                  description: "The SHA-256 fingerprint of the `RuntimePolicy` this run was\nauthorized under, so the violation is auditable against a\nspecific merge of org/repo/user/per-run layers.",
                  type: "string"
                },
                vendor_child_id: {
                  description: "Present only for a nested-worker violation; `None` for any\nviolation with no vendor child, such as a cost ceiling.",
                  type: [
                    "string",
                    "null"
                  ]
                },
                vendor_parent_ref: {
                  type: [
                    "string",
                    "null"
                  ]
                },
                action: {
                  type: "string"
                }
              },
              required: [
                "violation_id",
                "code",
                "observed_event_sequence",
                "policy_fingerprint",
                "action"
              ]
            }
          },
          required: [
            "policyViolationRecorded"
          ],
          additionalProperties: false
        },
        {
          description: "A policy violation was resolved (decided) by the owning OMP client.",
          type: "object",
          properties: {
            policyViolationDecided: {
              type: "object",
              properties: {
                violation_id: {
                  $ref: "#/$defs/PolicyViolationId"
                },
                resolution: {
                  type: "string"
                },
                resolved_by: {
                  type: "string"
                }
              },
              required: [
                "violation_id",
                "resolution",
                "resolved_by"
              ]
            }
          },
          required: [
            "policyViolationDecided"
          ],
          additionalProperties: false
        }
      ]
    },
    PolicyViolationId: {
      description: "Identifies a mid-run nested-worker policy violation.",
      type: "string"
    },
    RunFlags: {
      description: "Independent boolean flags on a run.\n\n`degradedControl`, `needsReconciliation`, `protocolUnhealthy`,\n`policyQuarantined`, `workspaceDirty`, and `childrenActive` are all\nindependent booleans.",
      type: "object",
      properties: {
        degradedControl: {
          type: "boolean"
        },
        needsReconciliation: {
          type: "boolean"
        },
        protocolUnhealthy: {
          type: "boolean"
        },
        policyQuarantined: {
          type: "boolean"
        },
        workspaceDirty: {
          type: "boolean"
        },
        childrenActive: {
          type: "boolean"
        }
      },
      additionalProperties: false,
      required: [
        "degradedControl",
        "needsReconciliation",
        "protocolUnhealthy",
        "policyQuarantined",
        "workspaceDirty",
        "childrenActive"
      ]
    },
    MessageId: {
      description: "Identifies a single message within a run's transcript.",
      type: "string"
    },
    ApprovalId: {
      description: "Identifies an approval request raised by the runtime.",
      type: "string"
    },
    DecidedBy: {
      description: "Who produced an approval decision. Sent by `approval/decide` and\nenforced by the runtime: an approval created with\n`human_required: true` may only be decided by [`DecidedBy::Human`].",
      oneOf: [
        {
          description: "A human answered an interactive dialog.",
          type: "string",
          const: "human"
        },
        {
          description: "The calling model supplied the decision itself.",
          type: "string",
          const: "model"
        }
      ]
    },
    ArtifactId: {
      description: "Identifies an artifact produced by a run.",
      type: "string"
    },
    WorkspaceEvent: {
      description: 'A workspace lease lifecycle event produced by `acquire`/`release`/`inspect`/`apply`.\n\nSerialized as an adjacently tagged enum: `{ "type": "...", "payload": { ... } }`.',
      oneOf: [
        {
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "leaseRequested"
            },
            payload: {
              type: "object",
              properties: {
                leaseId: {
                  type: "string"
                },
                runId: {
                  $ref: "#/$defs/RunId"
                },
                mode: {
                  $ref: "#/$defs/LeaseMode"
                }
              },
              additionalProperties: false,
              required: [
                "leaseId",
                "runId",
                "mode"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "leaseAcquired"
            },
            payload: {
              type: "object",
              properties: {
                leaseId: {
                  type: "string"
                },
                runId: {
                  $ref: "#/$defs/RunId"
                },
                path: {
                  type: "string"
                },
                isolationKind: {
                  $ref: "#/$defs/IsolationKind"
                },
                baseRevision: {
                  type: "string"
                }
              },
              additionalProperties: false,
              required: [
                "leaseId",
                "runId",
                "path",
                "isolationKind",
                "baseRevision"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "workspaceDirty"
            },
            payload: {
              type: "object",
              properties: {
                leaseId: {
                  type: "string"
                },
                dirtyFileCount: {
                  type: "integer",
                  format: "uint64",
                  minimum: 0
                },
                untrackedFileCount: {
                  type: "integer",
                  format: "uint64",
                  minimum: 0
                }
              },
              additionalProperties: false,
              required: [
                "leaseId",
                "dirtyFileCount",
                "untrackedFileCount"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "workspaceInspected"
            },
            payload: {
              type: "object",
              properties: {
                leaseId: {
                  type: "string"
                },
                patchArtifactId: {
                  $ref: "#/$defs/ArtifactId"
                },
                commitCount: {
                  type: "integer",
                  format: "uint64",
                  minimum: 0
                },
                dirtyFileCount: {
                  type: "integer",
                  format: "uint64",
                  minimum: 0
                },
                untrackedFileCount: {
                  type: "integer",
                  format: "uint64",
                  minimum: 0
                }
              },
              additionalProperties: false,
              required: [
                "leaseId",
                "patchArtifactId",
                "commitCount",
                "dirtyFileCount",
                "untrackedFileCount"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "applyStarted"
            },
            payload: {
              type: "object",
              properties: {
                leaseId: {
                  type: "string"
                },
                strategy: {
                  $ref: "#/$defs/ApplyStrategy"
                },
                artifactId: {
                  $ref: "#/$defs/ArtifactId"
                },
                expectedTargetRevision: {
                  type: "string"
                }
              },
              additionalProperties: false,
              required: [
                "leaseId",
                "strategy",
                "artifactId",
                "expectedTargetRevision"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "applyCompleted"
            },
            payload: {
              type: "object",
              properties: {
                leaseId: {
                  type: "string"
                },
                success: {
                  type: "boolean"
                },
                conflictArtifactId: {
                  anyOf: [
                    {
                      $ref: "#/$defs/ArtifactId"
                    },
                    {
                      type: "null"
                    }
                  ]
                },
                targetRevisionAfter: {
                  type: [
                    "string",
                    "null"
                  ]
                }
              },
              additionalProperties: false,
              required: [
                "leaseId",
                "success"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "leaseReleased"
            },
            payload: {
              type: "object",
              properties: {
                leaseId: {
                  type: "string"
                },
                runId: {
                  $ref: "#/$defs/RunId"
                }
              },
              additionalProperties: false,
              required: [
                "leaseId",
                "runId"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "cleanupFailed"
            },
            payload: {
              type: "object",
              properties: {
                leaseId: {
                  type: "string"
                },
                error: {
                  type: "string"
                }
              },
              additionalProperties: false,
              required: [
                "leaseId",
                "error"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          description: "An artifact was published for a workspace (inspect or apply produced one).",
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "artifactPublished"
            },
            payload: {
              type: "object",
              properties: {
                leaseId: {
                  type: "string"
                },
                artifactId: {
                  $ref: "#/$defs/ArtifactId"
                },
                kind: {
                  type: "string"
                }
              },
              additionalProperties: false,
              required: [
                "leaseId",
                "artifactId",
                "kind"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        },
        {
          description: "An apply attempt produced a conflict that OMP must resolve.",
          type: "object",
          properties: {
            type: {
              type: "string",
              const: "applyConflict"
            },
            payload: {
              type: "object",
              properties: {
                leaseId: {
                  type: "string"
                },
                conflictArtifactId: {
                  $ref: "#/$defs/ArtifactId"
                },
                strategy: {
                  $ref: "#/$defs/ApplyStrategy"
                },
                expectedTargetRevision: {
                  type: "string"
                }
              },
              additionalProperties: false,
              required: [
                "leaseId",
                "conflictArtifactId",
                "strategy",
                "expectedTargetRevision"
              ]
            }
          },
          required: [
            "type",
            "payload"
          ],
          additionalProperties: false
        }
      ]
    },
    LeaseMode: {
      description: "The isolation mode requested for a workspace lease.\n\n`shared` allows multiple read-only workers to share one path;\n`write` requires exclusive isolation (git-worktree or copy).",
      type: "string",
      enum: [
        "readOnly",
        "write"
      ]
    },
    IsolationKind: {
      description: "The isolation strategy used to materialize a workspace.\n\n`shared` means no isolation (single path shared by read-only workers);\n`gitWorktree` uses `git worktree add --detach`; `copy` copies the tree\nwithout following symlinks or copying `.git` administrative data.",
      type: "string",
      enum: [
        "shared",
        "gitWorktree",
        "copy"
      ]
    },
    ApplyStrategy: {
      description: "The mechanical strategy for applying a workspace change.",
      type: "string",
      enum: [
        "applyPatch",
        "cherryPick"
      ]
    },
    DisplayBackend: {
      description: "Supported display backends.",
      oneOf: [
        {
          description: "Herdr terminal multiplexer backend.",
          type: "string",
          const: "herdr"
        },
        {
          description: "Tmux terminal multiplexer backend.",
          type: "string",
          const: "tmux"
        },
        {
          description: "Raw terminal backend (degraded capabilities).",
          type: "string",
          const: "terminal"
        }
      ]
    },
    DisplayPlacement: {
      description: `Where a display backend places a pane relative to the caller's own
terminal. Changes presentation only; never run ownership.`,
      oneOf: [
        {
          description: "Rendered inside the caller's own OMP session, no separate pane.",
          type: "string",
          const: "embedded"
        },
        {
          description: "A new pane split to the right of the current one.",
          type: "string",
          const: "splitRight"
        },
        {
          description: "A new pane split below the current one.",
          type: "string",
          const: "splitDown"
        },
        {
          description: "A new tab.",
          type: "string",
          const: "tab"
        },
        {
          description: "A new workspace (Herdr only; unsupported by tmux).",
          type: "string",
          const: "workspace"
        }
      ]
    },
    DisplayConfig: {
      description: "Display configuration.",
      type: "object",
      properties: {
        backend: {
          description: "The backend to use.",
          $ref: "#/$defs/DisplayBackend"
        },
        width: {
          description: "Optional width override (None = auto-detect).",
          type: [
            "integer",
            "null"
          ],
          format: "uint16",
          minimum: 0,
          maximum: 65535
        },
        height: {
          description: "Optional height override (None = auto-detect).",
          type: [
            "integer",
            "null"
          ],
          format: "uint16",
          minimum: 0,
          maximum: 65535
        }
      },
      additionalProperties: false,
      required: [
        "backend"
      ]
    },
    DisplayStatus: {
      description: "Display status information.",
      type: "object",
      properties: {
        backend: {
          description: "The backend in use.",
          $ref: "#/$defs/DisplayBackend"
        },
        available: {
          description: "Whether the backend is available.",
          type: "boolean"
        },
        active: {
          description: "Whether the backend is currently active.",
          type: "boolean"
        },
        dimensions: {
          description: "Terminal dimensions if known.",
          type: [
            "array",
            "null"
          ],
          prefixItems: [
            {
              type: "integer",
              format: "uint16",
              minimum: 0,
              maximum: 65535
            },
            {
              type: "integer",
              format: "uint16",
              minimum: 0,
              maximum: 65535
            }
          ],
          minItems: 2,
          maxItems: 2
        }
      },
      additionalProperties: false,
      required: [
        "backend",
        "available",
        "active"
      ]
    },
    JsonRpcRequest: {
      description: "A JSON-RPC 2.0 request envelope.",
      type: "object",
      properties: {
        jsonrpc: {
          type: "string"
        },
        id: {
          $ref: "#/$defs/RequestId"
        },
        method: {
          type: "string"
        },
        params: true
      },
      additionalProperties: false,
      required: [
        "jsonrpc",
        "id",
        "method"
      ]
    },
    RequestId: {
      description: "A JSON-RPC request identifier, either a number or a string.",
      anyOf: [
        {
          type: "integer",
          format: "int64"
        },
        {
          type: "string"
        }
      ]
    },
    JsonRpcResponse: {
      description: "A JSON-RPC 2.0 success response envelope.",
      type: "object",
      properties: {
        jsonrpc: {
          type: "string"
        },
        id: {
          $ref: "#/$defs/RequestId"
        },
        result: true
      },
      additionalProperties: false,
      required: [
        "jsonrpc",
        "id",
        "result"
      ]
    },
    JsonRpcErrorResponse: {
      description: "A JSON-RPC 2.0 error response envelope. `id` is `None` when the request\nidentifier could not be determined (for example, on a parse error).",
      type: "object",
      properties: {
        jsonrpc: {
          type: "string"
        },
        id: {
          anyOf: [
            {
              $ref: "#/$defs/RequestId"
            },
            {
              type: "null"
            }
          ]
        },
        error: {
          $ref: "#/$defs/JsonRpcError"
        }
      },
      additionalProperties: false,
      required: [
        "jsonrpc",
        "error"
      ]
    },
    JsonRpcError: {
      description: "A JSON-RPC 2.0 error object. `data` carries optional, already-sanitized\ndiagnostic detail; it is omitted from the wire form when absent.",
      type: "object",
      properties: {
        code: {
          type: "integer",
          format: "int32"
        },
        message: {
          type: "string"
        },
        data: true
      },
      additionalProperties: false,
      required: [
        "code",
        "message"
      ]
    },
    JsonRpcNotification: {
      description: "A JSON-RPC 2.0 notification envelope: a method call with no `id`, for\nwhich no response is expected. BATMAN uses these to push runtime events to\nsubscribed clients via the `events/event` method.",
      type: "object",
      properties: {
        jsonrpc: {
          type: "string"
        },
        method: {
          type: "string"
        },
        params: true
      },
      additionalProperties: false,
      required: [
        "jsonrpc",
        "method"
      ]
    },
    RuntimeStatus: {
      description: "Result of a `runtime/status` request: a snapshot of the runtime's health\nand identity. Kept intentionally small at foundation scope; later tasks\nextend it with richer run/queue detail.",
      type: "object",
      properties: {
        running: {
          description: "Whether the runtime is accepting connections and serving requests.",
          type: "boolean"
        },
        protocol: {
          description: "The protocol version the runtime negotiated for this session.",
          $ref: "#/$defs/ProtocolVersion"
        },
        projectId: {
          description: "The canonical project id this runtime serves.",
          $ref: "#/$defs/ProjectId"
        },
        activeRuns: {
          description: "Number of runs the runtime's adapter registry is actively driving.",
          type: "integer",
          format: "uint32",
          minimum: 0
        },
        schemaVersion: {
          description: "The durable database schema version currently applied.",
          type: "integer",
          format: "uint32",
          minimum: 0
        },
        protocolHealthy: {
          description: `Whether the negotiated protocol is within the runtime's supported
range (a self-check that always holds for a live, negotiated session).`,
          type: "boolean"
        },
        uptimeSeconds: {
          description: "Seconds the runtime has been up since it started serving.",
          type: "integer",
          format: "uint64",
          minimum: 0
        },
        binarySource: {
          description: "Where the running binary was loaded from.",
          $ref: "#/$defs/BinarySource"
        }
      },
      additionalProperties: false,
      required: [
        "running",
        "protocol",
        "projectId",
        "activeRuns",
        "schemaVersion",
        "protocolHealthy",
        "uptimeSeconds",
        "binarySource"
      ]
    },
    BinarySource: {
      description: "Where the running `crewd` binary was loaded from. `override` means a\ndeveloper override path, `package` a bundled/installed binary, and\n`unknown` that the source could not be determined.",
      type: "string",
      enum: [
        "override",
        "package",
        "unknown"
      ]
    },
    ArtifactListResult: {
      description: "Result of listing artifacts.",
      type: "object",
      properties: {
        artifacts: {
          type: "array",
          items: {
            $ref: "#/$defs/Artifact"
          }
        }
      },
      additionalProperties: false,
      required: [
        "artifacts"
      ]
    },
    Artifact: {
      description: "Metadata for a stored artifact.",
      type: "object",
      properties: {
        artifactId: {
          $ref: "#/$defs/ArtifactId"
        },
        kind: {
          $ref: "#/$defs/ArtifactKind"
        },
        sha256: {
          type: "string"
        },
        byteLength: {
          type: "integer",
          format: "uint64",
          minimum: 0
        },
        mediaType: {
          type: "string"
        },
        storagePath: {
          description: "The relative storage path under the artifacts directory.",
          type: "string"
        },
        runId: {
          description: "The run ID that produced this artifact, if known.",
          type: [
            "string",
            "null"
          ]
        }
      },
      additionalProperties: false,
      required: [
        "artifactId",
        "kind",
        "sha256",
        "byteLength",
        "mediaType",
        "storagePath"
      ]
    },
    ArtifactKind: {
      description: "The kind of an artifact.",
      type: "string",
      enum: [
        "patch",
        "commitList",
        "conflictReport",
        "workspaceManifest"
      ]
    },
    ArtifactFetchResult: {
      description: "Result of fetching an artifact.",
      type: "object",
      properties: {
        artifact: {
          $ref: "#/$defs/Artifact"
        },
        contentBase64: {
          description: `Base64-encoded chunk of artifact bytes; callers decode explicitly.
Capped at 256 KiB per call.`,
          type: "string"
        },
        nextOffset: {
          type: [
            "integer",
            "null"
          ],
          format: "uint64",
          minimum: 0
        },
        complete: {
          type: "boolean"
        }
      },
      additionalProperties: false,
      required: [
        "artifact",
        "contentBase64",
        "complete"
      ]
    },
    InspectResult: {
      description: "Evidence captured by `inspect`: a binary-safe patch, commit list, and\ndirty/untracked state summary.",
      type: "object",
      properties: {
        leaseId: {
          type: "string"
        },
        patchArtifactId: {
          $ref: "#/$defs/ArtifactId"
        },
        commitCount: {
          type: "integer",
          format: "uint64",
          minimum: 0
        },
        commitIds: {
          type: "array",
          items: {
            type: "string"
          }
        },
        dirtyFileCount: {
          type: "integer",
          format: "uint64",
          minimum: 0
        },
        untrackedFileCount: {
          type: "integer",
          format: "uint64",
          minimum: 0
        },
        baseRevision: {
          type: "string"
        },
        currentRevision: {
          type: [
            "string",
            "null"
          ]
        }
      },
      additionalProperties: false,
      required: [
        "leaseId",
        "patchArtifactId",
        "commitCount",
        "commitIds",
        "dirtyFileCount",
        "untrackedFileCount",
        "baseRevision"
      ]
    },
    ApplyResult: {
      description: "Result of applying a workspace change.",
      type: "object",
      properties: {
        leaseId: {
          type: "string"
        },
        success: {
          type: "boolean"
        },
        conflictArtifactId: {
          anyOf: [
            {
              $ref: "#/$defs/ArtifactId"
            },
            {
              type: "null"
            }
          ]
        },
        targetRevisionAfter: {
          type: [
            "string",
            "null"
          ]
        },
        errorCode: {
          type: [
            "string",
            "null"
          ]
        }
      },
      additionalProperties: false,
      required: [
        "leaseId",
        "success"
      ]
    },
    WorkspaceInfo: {
      description: "Information about an active or recently-released lease, returned by `get`.",
      type: "object",
      properties: {
        leaseId: {
          type: "string"
        },
        runId: {
          $ref: "#/$defs/RunId"
        },
        mode: {
          $ref: "#/$defs/LeaseMode"
        },
        isolationKind: {
          $ref: "#/$defs/IsolationKind"
        },
        path: {
          type: "string"
        },
        state: {
          $ref: "#/$defs/WorkspaceState"
        },
        baseRevision: {
          type: "string"
        }
      },
      additionalProperties: false,
      required: [
        "leaseId",
        "runId",
        "mode",
        "isolationKind",
        "path",
        "state",
        "baseRevision"
      ]
    },
    WorkspaceState: {
      description: "The lifecycle state of a workspace lease.",
      type: "string",
      enum: [
        "allocating",
        "active",
        "dirty",
        "released",
        "cleanupFailed"
      ]
    },
    PolicyViolationListResult: {
      description: "Result of `policy/violation/list`.",
      type: "object",
      properties: {
        violations: {
          type: "array",
          items: {
            $ref: "#/$defs/PolicyViolationSummary"
          }
        }
      },
      additionalProperties: false,
      required: [
        "violations"
      ]
    },
    PolicyViolationSummary: {
      description: "One recorded policy violation, projected exactly from the\n`policy_violations` table: an undecided row (`resolution` null) on a\nquarantined run is the one holding the quarantine.",
      type: "object",
      properties: {
        violationId: {
          $ref: "#/$defs/PolicyViolationId"
        },
        runId: {
          type: "string"
        },
        taskId: {
          type: "string"
        },
        workerId: {
          type: "string"
        },
        vendorChildId: {
          description: `The vendor-reported child id, when the violation had a vendor
child at all (a cost ceiling does not).`,
          type: [
            "string",
            "null"
          ]
        },
        vendorParentRef: {
          type: [
            "string",
            "null"
          ]
        },
        action: {
          description: "The action policy applied when the violation was recorded\n(`quarantine`, `cancel`, `quarantineAndCancel`).",
          type: "string"
        },
        createdAt: {
          type: "string"
        },
        resolvedAt: {
          description: "Set once decided via `policy/violation/decide`.",
          type: [
            "string",
            "null"
          ]
        },
        resolution: {
          description: '`"release"` or `"cancel"` once decided; `None` while open.',
          type: [
            "string",
            "null"
          ]
        },
        resolvedBy: {
          type: [
            "string",
            "null"
          ]
        }
      },
      additionalProperties: false,
      required: [
        "violationId",
        "runId",
        "taskId",
        "workerId",
        "action",
        "createdAt"
      ]
    },
    RunResultResult: {
      description: "Result of `run/result`: a terminal run's final journaled output.\n\n`result_text: None` means the run journaled no visible final message\n(or it was fully redacted) -- distinct from an error. `usage: None`\nmeans the adapter never reported usage (e.g. Copilot under ACP v1).",
      type: "object",
      properties: {
        runId: {
          $ref: "#/$defs/RunId"
        },
        state: {
          $ref: "#/$defs/RunState"
        },
        resultText: {
          type: [
            "string",
            "null"
          ]
        },
        usage: {
          anyOf: [
            {
              $ref: "#/$defs/RunUsage"
            },
            {
              type: "null"
            }
          ]
        },
        completedAt: {
          type: [
            "string",
            "null"
          ]
        }
      },
      additionalProperties: false,
      required: [
        "runId",
        "state"
      ]
    },
    RunState: {
      description: "The lifecycle state of a run.\n\nOnly the runtime applies a transition after process/protocol evidence.\nTerminal states (`succeeded`, `failed`, `cancelled`, `lost`) have no\noutgoing edges.",
      type: "string"
    },
    RunUsage: {
      description: "Token usage folded from a run's journaled `AdapterUsageEvent`s.\n\nThe runtime applies the adapter-correct fold before this leaves the\ndaemon: Claude journals per-invocation deltas (summed); every other\nreporting adapter journals cumulative totals (last one wins). Codex\nnever reports cost, so `cost_usd` is `null` there.",
      type: "object",
      properties: {
        inputTokens: {
          type: "integer",
          format: "uint64",
          minimum: 0
        },
        outputTokens: {
          type: "integer",
          format: "uint64",
          minimum: 0
        },
        costUsd: {
          type: [
            "number",
            "null"
          ],
          format: "double"
        }
      },
      additionalProperties: false,
      required: [
        "inputTokens",
        "outputTokens"
      ]
    }
  }
};

// ../protocol-ts/src/validate.ts
var SCHEMA_ID = "https://schema.batman.satorianalytics.com/batman.schema.json";
var ajv = new import__2020.default({
  strict: true,
  allErrors: true,
  coerceTypes: false,
  removeAdditional: false,
  useDefaults: false
});
for (const format of ["int16", "uint16", "int32", "uint32", "int64", "uint64", "float", "double"]) {
  ajv.addFormat(format, true);
}
ajv.addSchema({ ...batman_schema_default, $id: SCHEMA_ID });
function def(name) {
  const validate = ajv.getSchema(`${SCHEMA_ID}#/$defs/${name}`);
  if (validate === undefined) {
    throw new Error(`schema is missing the expected $def: ${name}`);
  }
  return validate;
}
var validateInitializeResult = def("InitializeResult");
var validateRuntimeStatus = def("RuntimeStatus");
var validateArtifactListResult = def("ArtifactListResult");
var validateArtifactFetchResult = def("ArtifactFetchResult");
var validateInspectResult = def("InspectResult");
var validateApplyResult = def("ApplyResult");
var validateWorkspaceInfo = def("WorkspaceInfo");
var validatePolicyViolationListResult = def("PolicyViolationListResult");
var validateEventEnvelope = def("EventEnvelope");
var validateJsonRpcResponse = def("JsonRpcResponse");
var validateJsonRpcErrorResponse = def("JsonRpcErrorResponse");
var validateJsonRpcNotification = def("JsonRpcNotification");
var validateRunResultResult = def("RunResultResult");
var validateEventEnvelopeArray = ajv.compile({
  $id: "https://schema.batman.satorianalytics.com/event-envelope-array.json",
  type: "array",
  items: { $ref: `${SCHEMA_ID}#/$defs/EventEnvelope` }
});

class ValidationError extends Error {
  what;
  errors;
  constructor(what, errors) {
    super(`${what} failed schema validation: ${JSON.stringify(errors)}`);
    this.name = "ValidationError";
    this.what = what;
    this.errors = errors;
  }
}
function assertValid(validate, data, what) {
  if (!validate(data)) {
    throw new ValidationError(what, validate.errors ?? null);
  }
}

// src/client.ts
var BOOTSTRAP_MAX_FRAME_BYTES = 4 * 1024 * 1024;
var EVENTS_EVENT_METHOD = "events/event";
var RESULT_VALIDATORS = {
  "runtime/status": validateRuntimeStatus,
  "artifact/list": validateArtifactListResult,
  "artifact/fetch": validateArtifactFetchResult,
  "workspace/inspect": validateInspectResult,
  "workspace/apply": validateApplyResult,
  "workspace/get": validateWorkspaceInfo,
  "policy/violation/list": validatePolicyViolationListResult,
  "run/result": validateRunResultResult
};

class JsonRpcRemoteError extends Error {
  code;
  data;
  constructor(code, message, data) {
    super(message);
    this.name = "JsonRpcRemoteError";
    this.code = code;
    this.data = data;
  }
}
class CrewClient {
  #socket;
  #buffer = "";
  #maxFrameBytes = BOOTSTRAP_MAX_FRAME_BYTES;
  #nextId = 1;
  #initialized = false;
  #closed = false;
  #closeReason;
  #pending = new Map;
  #subscribers = new Set;
  #ready;
  constructor(options) {
    this.#socket = createConnection({ path: options.socketPath });
    this.#socket.setEncoding("utf8");
    this.#ready = new Promise((resolve, reject) => {
      this.#socket.once("connect", () => resolve());
      this.#socket.once("error", (err) => reject(err));
    });
    this.#socket.on("data", (chunk) => this.#onData(chunk));
    this.#socket.on("close", () => this.#onClose());
    this.#socket.on("error", (err) => this.#onError(err));
  }
  async initialize(params) {
    const result = await this.#send("initialize", params);
    assertValid(validateInitializeResult, result, "initialize result");
    this.#maxFrameBytes = result.capabilities.maxFrameBytes;
    this.#initialized = true;
    return result;
  }
  async request(method, params) {
    if (!this.#initialized && method !== "initialize") {
      throw new Error(`cannot call ${method} before initialize()`);
    }
    const result = await this.#send(method, params);
    const validator = RESULT_VALIDATORS[method];
    if (validator !== undefined) {
      assertValid(validator, result, `${method} result`);
    } else if (!isObject(result)) {
      throw new ValidationError(`${method} result`, [{ message: "result is not a JSON object" }]);
    }
    return result;
  }
  subscribe(fromSequence, onEvent) {
    this.#subscribers.add(onEvent);
    (async () => {
      try {
        const replayed = await this.#send("events/replay", { afterSequence: fromSequence });
        assertValid(validateEventEnvelopeArray, replayed, "events/replay result");
        for (const event of replayed) {
          onEvent(event);
        }
        await this.#send("events/subscribe", {});
      } catch (err) {
        this.#socket.emit("error", err instanceof Error ? err : new Error(String(err)));
      }
    })();
    return () => {
      this.#subscribers.delete(onEvent);
    };
  }
  close() {
    if (this.#closed) {
      return;
    }
    this.#closed = true;
    this.#socket.destroy();
    this.#failPending(new Error("client closed"));
  }
  get isClosed() {
    return this.#closed;
  }
  #send(method, params) {
    return new Promise((resolve, reject) => {
      if (this.#closed) {
        reject(this.#closeReason ?? new Error("client is closed"));
        return;
      }
      const id = String(this.#nextId++);
      const frame = JSON.stringify({ jsonrpc: "2.0", id, method, params });
      const frameBytes = Buffer.byteLength(frame, "utf8");
      if (frameBytes + 1 > this.#maxFrameBytes) {
        reject(new Error(`outbound frame of ${frameBytes + 1} bytes exceeds the negotiated maximum of ${this.#maxFrameBytes}`));
        return;
      }
      this.#pending.set(id, { method, resolve, reject });
      this.#socket.write(`${frame}
`, (err) => {
        if (err) {
          this.#pending.delete(id);
          reject(err);
        }
      });
    });
  }
  #onData(chunk) {
    this.#buffer += chunk;
    let newline = this.#buffer.indexOf(`
`);
    while (newline !== -1) {
      const line = this.#buffer.slice(0, newline);
      this.#buffer = this.#buffer.slice(newline + 1);
      if (line.length > 0) {
        const lineBytes = Buffer.byteLength(line, "utf8");
        if (lineBytes + 1 > this.#maxFrameBytes) {
          this.#onError(new Error(`inbound frame of ${lineBytes + 1} bytes exceeds the negotiated maximum of ${this.#maxFrameBytes}`));
          return;
        }
        this.#handleLine(line);
      }
      newline = this.#buffer.indexOf(`
`);
    }
    if (Buffer.byteLength(this.#buffer, "utf8") > this.#maxFrameBytes) {
      this.#onError(new Error(`inbound frame exceeds the ${this.#maxFrameBytes}-byte maximum with no frame boundary`));
    }
  }
  #handleLine(line) {
    let message;
    try {
      message = JSON.parse(line);
    } catch {
      this.#onError(new Error("received a frame that is not valid JSON"));
      return;
    }
    if (!isObject(message)) {
      this.#onError(new Error("received a non-object JSON-RPC message"));
      return;
    }
    if (message.id === undefined && typeof message.method === "string") {
      this.#handleNotification(message);
      return;
    }
    this.#handleResponse(message);
  }
  #handleNotification(message) {
    try {
      assertValid(validateJsonRpcNotification, message, "notification envelope");
    } catch (err) {
      this.#onError(err instanceof Error ? err : new Error(String(err)));
      return;
    }
    if (message.method === EVENTS_EVENT_METHOD) {
      const params = message.params;
      try {
        assertValid(validateEventEnvelope, params, "event notification");
      } catch (err) {
        this.#onError(err instanceof Error ? err : new Error(String(err)));
        return;
      }
      for (const subscriber of this.#subscribers) {
        subscriber(params);
      }
    }
  }
  #handleResponse(message) {
    const id = typeof message.id === "string" ? message.id : String(message.id);
    const pending = this.#pending.get(id);
    if (pending === undefined) {
      this.#onError(new Error(`received a response for unknown id ${id}`));
      return;
    }
    this.#pending.delete(id);
    if ("error" in message) {
      try {
        assertValid(validateJsonRpcErrorResponse, message, "error response envelope");
      } catch (err) {
        pending.reject(err instanceof Error ? err : new Error(String(err)));
        return;
      }
      const error = message.error;
      pending.reject(new JsonRpcRemoteError(error.code, error.message, error.data));
      return;
    }
    try {
      assertValid(validateJsonRpcResponse, message, "success response envelope");
    } catch (err) {
      pending.reject(err instanceof Error ? err : new Error(String(err)));
      return;
    }
    pending.resolve(message.result);
  }
  #onError(err) {
    this.#closeReason ??= err;
    this.#failPending(err);
    if (!this.#closed) {
      this.#closed = true;
      this.#socket.destroy();
    }
  }
  #onClose() {
    this.#closed = true;
    this.#failPending(this.#closeReason ?? new Error("connection closed by runtime"));
  }
  #failPending(reason) {
    for (const pending of this.#pending.values()) {
      pending.reject(reason);
    }
    this.#pending.clear();
  }
  whenConnected() {
    return this.#ready;
  }
}
function isObject(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

// src/env-flag.ts
function envFlag(env, newName, oldName) {
  return env[newName] ?? env[oldName];
}
// package.json
var package_default = {
  name: "@nikolasd/crew",
  version: "0.4.1",
  type: "module",
  exports: { ".": "./dist/index.js" },
  omp: { extensions: ["./dist/index.js"] },
  scripts: {
    build: "bun build src/index.ts --outdir dist --target bun --external @oh-my-pi/pi-coding-agent"
  },
  peerDependencies: { "@oh-my-pi/pi-coding-agent": ">=17.0.7 <18" },
  devDependencies: {
    "@oh-my-pi/pi-coding-agent": ">=17.0.7 <18",
    "@nikolasd/batman-protocol": "workspace:*",
    "@types/bun": "1.3.14",
    ajv: "8.17.1",
    zod: "^4"
  }
};

// src/runtime.ts
var CONNECT_MAX_FRAME_BYTES = 1024 * 1024;
var CONNECT_DEADLINE_MS = 5000;

class BinarySelectionError extends Error {
  code;
  constructor(code, message) {
    super(message);
    this.name = "BinarySelectionError";
    this.code = code;
  }
}
function buildServeArgs(options) {
  return ["serve", "--state-dir", options.stateDir, "--repo", options.repository, "--idle-seconds", String(options.idleSeconds)];
}
async function ensureRuntime(options) {
  const socketPath = socketPathFor(options.stateDir, options.repository);
  const existing = await tryConnect(socketPath, options.repository, options.sessionId);
  if (existing !== undefined) {
    return { client: existing, childStarted: false };
  }
  const binary = selectBinary(options.env ?? process.env, options.packagedBinaryResolver);
  const child = spawn(binary.path, buildServeArgs(options), {
    detached: true,
    stdio: "ignore",
    env: { ...process.env, CREW_BINARY_SOURCE: binary.source }
  });
  child.on("error", (err) => {
    console.error(`crew runtime: failed to spawn ${binary.path}: ${err instanceof Error ? err.message : String(err)}`);
  });
  child.unref();
  const client = await connectWithBackoff(socketPath, options.repository, options.sessionId);
  return { client, childStarted: true };
}
function socketPathFor(stateDir, repository) {
  return join(stateDir, "repos", repositoryId(repository), "runtime.sock");
}
function repositoryId(repository) {
  const canonical = realpathSync(repository);
  const vcsRoot = discoverVcsRoot(canonical) ?? canonical;
  return repositoryIdFromRoot(vcsRoot);
}
function repositoryIdFromRoot(canonicalRoot) {
  const digest = createHash("sha256").update(canonicalRoot, "utf8").digest();
  return digest.subarray(0, 16).toString("hex");
}
function discoverVcsRoot(canonical) {
  let current = canonical;
  for (;; ) {
    if (pathExists(join(current, ".git"))) {
      return current;
    }
    const parent = dirname(current);
    if (parent === current) {
      return;
    }
    current = parent;
  }
}
function pathExists(path) {
  try {
    lstatSync(path);
    return true;
  } catch {
    return false;
  }
}
function resolveOverride(env) {
  const override = envFlag(env, "OMP_CREW_BINARY", "OMP_BATMAN_BINARY");
  if (override === undefined || override === "") {
    return;
  }
  if (!isAbsolute(override)) {
    throw new BinarySelectionError("not-absolute", `OMP_CREW_BINARY must be an absolute path, got ${JSON.stringify(override)}`);
  }
  let canonical;
  try {
    canonical = realpathSync(override);
  } catch {
    throw new BinarySelectionError("not-found", `OMP_CREW_BINARY does not exist: ${override}`);
  }
  const stat = statSync(canonical);
  if (!stat.isFile()) {
    throw new BinarySelectionError("not-regular", `OMP_CREW_BINARY is not a regular file: ${override}`);
  }
  if ((stat.mode & 73) === 0) {
    throw new BinarySelectionError("not-executable", `OMP_CREW_BINARY is not executable: ${override}`);
  }
  return { path: override, source: "override" };
}
function selectBinary(env, packagedBinaryResolver) {
  const override = resolveOverride(env);
  if (override !== undefined) {
    return override;
  }
  if (packagedBinaryResolver !== undefined) {
    return { path: packagedBinaryResolver(), source: "package" };
  }
  throw new BinarySelectionError("no-binary", "no OMP_CREW_BINARY override is set and no packaged-binary resolver was provided");
}
function initParams(repository, sessionId) {
  const canonical = realpathSync(repository);
  return {
    client: { name: "@nikolasd/crew", version: package_default.version },
    supported: { min: { major: 1, minor: 0 }, max: { major: 1, minor: 0 } },
    repository: { canonicalPath: canonical, vcsRoot: canonical },
    auth: { role: "ompExtension", instanceId: sessionId ?? "crew-extension", agentDirectory: canonical },
    capabilities: { eventReplay: false, maxFrameBytes: CONNECT_MAX_FRAME_BYTES },
    lastSequence: null
  };
}
async function tryConnect(socketPath, repository, sessionId) {
  if (!existsSync(socketPath)) {
    return;
  }
  const client = new CrewClient({ socketPath });
  try {
    await client.whenConnected();
    await client.initialize(initParams(repository, sessionId));
    return client;
  } catch {
    client.close();
    return;
  }
}
async function connectWithBackoff(socketPath, repository, sessionId) {
  const deadline = Date.now() + CONNECT_DEADLINE_MS;
  let delay = 25;
  for (;; ) {
    const client = await tryConnect(socketPath, repository, sessionId);
    if (client !== undefined) {
      return client;
    }
    if (Date.now() >= deadline) {
      throw new Error(`runtime did not become reachable at ${socketPath} within ${CONNECT_DEADLINE_MS}ms`);
    }
    await sleep(Math.min(delay, deadline - Date.now()));
    delay = Math.min(delay * 2, 500);
  }
}
function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, Math.max(0, ms)));
}

// src/integrity.ts
import { createHash as createHash2 } from "crypto";
import { readFileSync } from "fs";
function sha256File(path) {
  return createHash2("sha256").update(readFileSync(path)).digest("hex");
}

// src/platform.ts
var EXTENSION_VERSION = package_default.version;
var SUPPORTED_TARGETS = ["darwin-arm64", "darwin-x64", "linux-arm64-gnu", "linux-x64-gnu"];

class UnsupportedPlatformError extends Error {
  code = "unsupported-platform";
  platform;
  arch;
  libc;
  constructor(platform, arch, libc) {
    super(`unsupported platform: platform=${platform} arch=${arch} libc=${libc ?? "unknown"} ` + `(supported: ${SUPPORTED_TARGETS.join(", ")})`);
    this.name = "UnsupportedPlatformError";
    this.platform = platform;
    this.arch = arch;
    this.libc = libc;
  }
}

class BinaryIntegrityError extends Error {
  code;
  constructor(code, message) {
    super(message);
    this.name = "BinaryIntegrityError";
    this.code = code;
  }
}
function resolveTarget(platform, arch, libc) {
  const target = mapTarget(platform, arch, libc);
  if (target === undefined) {
    throw new UnsupportedPlatformError(platform, arch, libc);
  }
  return target;
}
function runtimeCacheDir(stateRoot, version) {
  return join2(stateRoot, "bin", version);
}
function resolveCrewd(platform, arch, libc, env, stateRoot) {
  const override = resolveOverride(env);
  if (override !== undefined) {
    return override;
  }
  const target = resolveTarget(platform, arch, libc);
  const dir = runtimeCacheDir(stateRoot, EXTENSION_VERSION);
  const binPath = join2(dir, "crewd");
  const manifestPath = join2(dir, "manifest.json");
  if (!existsSync2(binPath) || !existsSync2(manifestPath)) {
    throw new BinarySelectionError("runtime-not-installed", `no crewd binary installed for version ${EXTENSION_VERSION}; run /crew-runtime-install to download it, or set OMP_CREW_BINARY to a local build`);
  }
  const manifest = readManifest(manifestPath);
  if (manifest.target !== target) {
    throw new BinaryIntegrityError("manifest-invalid", `manifest at ${manifestPath} declares target ${manifest.target}, but this platform requires ${target}`);
  }
  const actualSha256 = sha256File(binPath);
  if (actualSha256 !== manifest.sha256) {
    throw new BinaryIntegrityError("checksum-mismatch", `checksum mismatch for ${binPath}: manifest ${manifestPath} declares ${manifest.sha256}, ` + `computed ${actualSha256}`);
  }
  if (manifest.version !== EXTENSION_VERSION) {
    throw new BinaryIntegrityError("version-mismatch", `cached binary is version ${manifest.version}, but this extension is version ${EXTENSION_VERSION}`);
  }
  return { path: binPath, source: "package" };
}
function parseManifest(raw, sourceLabel) {
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch (err) {
    throw new BinaryIntegrityError("manifest-invalid", `manifest at ${sourceLabel} is not valid JSON: ${err.message}`);
  }
  if (typeof parsed !== "object" || parsed === null || typeof parsed.sha256 !== "string" || typeof parsed.version !== "string" || typeof parsed.target !== "string" || typeof parsed.sizeBytes !== "number") {
    throw new BinaryIntegrityError("manifest-invalid", `manifest at ${sourceLabel} is missing required fields "sha256"/"version"/"target"/"sizeBytes"`);
  }
  return parsed;
}
function readManifest(manifestPath) {
  let raw;
  try {
    raw = readFileSync2(manifestPath, "utf8");
  } catch (err) {
    throw new BinaryIntegrityError("manifest-invalid", `unable to read manifest at ${manifestPath}: ${err.message}`);
  }
  return parseManifest(raw, manifestPath);
}
function mapTarget(platform, arch, libc) {
  if (platform === "darwin" && arch === "arm64") {
    return "darwin-arm64";
  }
  if (platform === "darwin" && arch === "x64") {
    return "darwin-x64";
  }
  if (platform === "linux" && arch === "arm64" && libc === "glibc") {
    return "linux-arm64-gnu";
  }
  if (platform === "linux" && arch === "x64" && libc === "glibc") {
    return "linux-x64-gnu";
  }
  return;
}
function detectLibc(platform = process.platform) {
  if (platform !== "linux") {
    return;
  }
  try {
    const report = process.report?.getReport()?.header;
    if (report?.glibcVersionRuntime) {
      return "glibc";
    }
  } catch {}
  const muslLoaders = ["/lib/ld-musl-x86_64.so.1", "/lib/ld-musl-aarch64.so.1"];
  if (muslLoaders.some((loader) => existsSync2(loader))) {
    return "musl";
  }
  return;
}

// src/state.ts
import { isAbsolute as isAbsolute2, join as join3 } from "path";
class StateRootError extends Error {
  code;
  constructor(code, message) {
    super(message);
    this.name = "StateRootError";
    this.code = code;
  }
}
function resolveStateRoot(env, home) {
  const crewStateDir = envFlag(env, "CREW_STATE_DIR", "BATMAN_STATE_DIR");
  if (crewStateDir !== undefined) {
    if (!isAbsolute2(crewStateDir)) {
      throw new StateRootError("relative-override", `CREW_STATE_DIR must be an absolute path, got ${JSON.stringify(crewStateDir)}`);
    }
    return crewStateDir;
  }
  const xdgStateHome = env.XDG_STATE_HOME;
  if (xdgStateHome !== undefined) {
    if (!isAbsolute2(xdgStateHome)) {
      throw new StateRootError("relative-override", `XDG_STATE_HOME must be an absolute path, got ${JSON.stringify(xdgStateHome)}`);
    }
    return join3(xdgStateHome, "omp", "batman");
  }
  const piConfigDir = env.PI_CONFIG_DIR ?? ".omp";
  return join3(home, piConfigDir, "batman");
}

// src/context.ts
var DEFAULT_IDLE_SECONDS = 30 * 60;
function buildStatusContext(options = {}) {
  const env = options.env ?? process.env;
  const home = options.home ?? homedir();
  const repository = options.cwd ?? process.cwd();
  const stateDir = resolveStateRoot(env, home);
  return {
    ensureRuntimeOptions: {
      stateDir,
      repository,
      idleSeconds: DEFAULT_IDLE_SECONDS,
      env,
      packagedBinaryResolver: options.packagedBinaryResolver ?? (() => resolveCrewd(process.platform, process.arch, detectLibc(), env, stateDir).path),
      sessionId: options.sessionId
    }
  };
}

// src/omp-native/events.ts
function mapLifecycleStatus(status) {
  switch (status) {
    case "started":
      return "working";
    case "completed":
      return "succeeded";
    case "failed":
    case "aborted":
      return "failed";
  }
}
function mapProgressStatus(status) {
  switch (status) {
    case "pending":
    case "running":
      return "working";
    case "completed":
      return "succeeded";
    case "failed":
    case "aborted":
      return "failed";
  }
}
function normalizeLifecyclePayload(payload, ompProcessEpoch, observedAtMs) {
  return {
    ompAgentId: payload.id,
    status: mapLifecycleStatus(payload.status),
    description: payload.description,
    sessionFile: payload.sessionFile,
    artifactRefs: [],
    ompProcessEpoch,
    observedAtMs
  };
}
function normalizeProgressPayload(payload, ompProcessEpoch, observedAtMs) {
  return {
    ompAgentId: payload.progress.id,
    status: mapProgressStatus(payload.progress.status),
    description: payload.progress.description ?? payload.progress.assignment,
    sessionFile: payload.sessionFile,
    artifactRefs: [],
    ompProcessEpoch,
    observedAtMs
  };
}
function normalizeEventPayload(_payload) {
  return;
}

// src/omp-native/persistence.ts
var OMP_NATIVE_FACT_ENTRY_TYPE = "crew-omp-native-fact";
var OMP_NATIVE_CORRELATION_ENTRY_TYPE = "crew-omp-native-correlation";
function asFact(data) {
  if (data === null || typeof data !== "object")
    return;
  const { ompAgentId, status, ompProcessEpoch, observedAtMs, artifactRefs } = data;
  if (typeof ompAgentId !== "string" || typeof status !== "string")
    return;
  if (typeof ompProcessEpoch !== "string" || typeof observedAtMs !== "number")
    return;
  if (!Array.isArray(artifactRefs))
    return;
  if (status !== "working" && status !== "succeeded" && status !== "failed" && status !== "lost") {
    return;
  }
  const { description, sessionFile } = data;
  return {
    ompAgentId,
    status,
    ompProcessEpoch,
    observedAtMs,
    artifactRefs: artifactRefs.filter((ref) => typeof ref === "string"),
    ...typeof description === "string" ? { description } : {},
    ...typeof sessionFile === "string" ? { sessionFile } : {}
  };
}
function asCorrelation(data) {
  if (data === null || typeof data !== "object")
    return;
  const { taskId, revision } = data;
  if (typeof taskId !== "string" || typeof revision !== "number")
    return;
  return { taskId, revision };
}
function persistedFacts(entries) {
  const latest = new Map;
  for (const entry of entries) {
    if (entry?.type !== "custom" || entry.customType !== OMP_NATIVE_FACT_ENTRY_TYPE)
      continue;
    const fact = asFact(entry.data);
    if (fact !== undefined) {
      latest.set(fact.ompAgentId, fact);
    }
  }
  return [...latest.values()];
}
function persistedCorrelations(entries) {
  const latest = new Map;
  for (const entry of entries) {
    if (entry?.type !== "custom" || entry.customType !== OMP_NATIVE_CORRELATION_ENTRY_TYPE) {
      continue;
    }
    const correlation = asCorrelation(entry.data);
    if (correlation !== undefined) {
      const prior = latest.get(correlation.taskId);
      if (prior === undefined || correlation.revision >= prior.revision) {
        latest.set(correlation.taskId, correlation);
      }
    }
  }
  return [...latest.values()];
}

// src/omp-native/reconcile.ts
var PROGRESS_COALESCE_MS = 150;
var TERMINAL_STATUSES = new Set(["succeeded", "failed", "lost"]);

class OmpNativeReconciler {
  #facts = new Map;
  #pendingTimers = new Map;
  #onChange;
  constructor(onChange = () => {}) {
    this.#onChange = onChange;
  }
  record(fact) {
    const previous = this.#facts.get(fact.ompAgentId);
    if (previous !== undefined && TERMINAL_STATUSES.has(previous.status)) {
      return;
    }
    const pending = this.#pendingTimers.get(fact.ompAgentId);
    if (pending !== undefined) {
      clearTimeout(pending);
      this.#pendingTimers.delete(fact.ompAgentId);
    }
    if (TERMINAL_STATUSES.has(fact.status)) {
      this.#facts.set(fact.ompAgentId, fact);
      this.#onChange(fact);
      return;
    }
    this.#pendingTimers.set(fact.ompAgentId, setTimeout(() => {
      this.#pendingTimers.delete(fact.ompAgentId);
      this.#facts.set(fact.ompAgentId, fact);
      this.#onChange(fact);
    }, PROGRESS_COALESCE_MS));
  }
  get(ompAgentId) {
    return this.#facts.get(ompAgentId);
  }
  all() {
    return [...this.#facts.values()];
  }
  dispose() {
    for (const timer of this.#pendingTimers.values()) {
      clearTimeout(timer);
    }
    this.#pendingTimers.clear();
  }
}
function reconcileAcrossRestart(priorFacts, currentEpoch) {
  return priorFacts.map((fact) => {
    if (fact.ompProcessEpoch === currentEpoch || TERMINAL_STATUSES.has(fact.status)) {
      return fact;
    }
    return { ...fact, status: "lost", ompProcessEpoch: currentEpoch };
  });
}
function createOmpProcessEpoch() {
  return crypto.randomUUID();
}
async function reconcileWithRuntime(client, correlation) {
  if (correlation === undefined) {
    return;
  }
  return client.request("reconcile/omp", {
    taskId: correlation.taskId,
    revision: correlation.revision
  });
}

// src/status.ts
async function resolveClient(ctx) {
  const cached = ctx.cache.get();
  if (cached !== undefined) {
    if (!cached.isClosed) {
      return cached;
    }
    try {
      cached.close();
    } catch {}
    ctx.cache.set(undefined);
  }
  const { client } = await ensureRuntime(ctx.ensureRuntimeOptions);
  ctx.cache.set(client);
  return client;
}
var GENERIC_FAILURE_MESSAGE = "The Crew runtime is not reachable for this repository. Run the doctor command below for details.";
async function getRuntimeStatus(ctx) {
  let client;
  try {
    client = await resolveClient(ctx);
  } catch (err) {
    return failureResult(ctx.ensureRuntimeOptions, err);
  }
  try {
    const status = await client.request("runtime/status");
    return {
      content: [{ type: "text", text: formatStatus(status) }],
      details: status
    };
  } catch (err) {
    try {
      client.close();
    } catch {}
    ctx.cache.set(undefined);
    return failureResult(ctx.ensureRuntimeOptions, err);
  }
}
function failureResult(options, err) {
  const code = errorCode(err);
  const doctorCommand = `crewd status --repo ${options.repository}`;
  const message = code === "runtime-not-installed" ? "The Crew runtime binary is not installed yet. Run /crew-runtime-install to download and verify it." : GENERIC_FAILURE_MESSAGE;
  return {
    isError: true,
    content: [{ type: "text", text: message }],
    details: { code, message, doctorCommand }
  };
}
function errorCode(err) {
  if (err instanceof BinarySelectionError || err instanceof BinaryIntegrityError || err instanceof UnsupportedPlatformError) {
    return err.code;
  }
  return "connection-failed";
}
function formatStatus(status) {
  return [
    `Crew runtime: ${status.running ? "running" : "not running"}`,
    `Protocol: ${status.protocol.major}.${status.protocol.minor} (healthy: ${status.protocolHealthy})`,
    `Project: ${status.projectId}`,
    `Active runs: ${status.activeRuns}`,
    `Schema version: ${status.schemaVersion}`,
    `Uptime: ${status.uptimeSeconds}s`,
    `Binary source: ${status.binarySource}`
  ].join(`
`);
}

// src/doctor.ts
import { spawn as spawn2 } from "child_process";
import { homedir as homedir2 } from "os";
function buildDoctorContext(cwd, env = process.env) {
  const stateDir = resolveStateRoot(env, homedir2());
  const binary = resolveCrewd(process.platform, process.arch, detectLibc(), env, stateDir);
  return {
    stateDir,
    repository: cwd,
    crewdPath: binary.path
  };
}
async function runDoctorCommand(ctx) {
  return new Promise((resolve) => {
    const proc = spawn2(ctx.crewdPath, ["doctor", "--json", "--state-dir", ctx.stateDir, "--repo", ctx.repository], {
      stdio: ["ignore", "pipe", "pipe"]
    });
    let stdout = "";
    let stderr = "";
    proc.stdout.on("data", (chunk) => {
      stdout += chunk.toString();
    });
    proc.stderr.on("data", (chunk) => {
      stderr += chunk.toString();
    });
    proc.on("close", (code) => {
      const exitCode = code ?? 1;
      const doctorCommand = `${ctx.crewdPath} doctor --state-dir ${ctx.stateDir} --repo ${ctx.repository}`;
      if (exitCode !== 0) {
        let parsed;
        try {
          parsed = JSON.parse(stdout);
        } catch {
          resolve(failureResult2(ctx, "doctor-failed", stderr.trim() || `Doctor command exited with code ${exitCode}`, doctorCommand));
          return;
        }
        if (isDoctorResult(parsed)) {
          resolve({
            isError: true,
            content: [{ type: "text", text: formatDoctorOutput(parsed) }],
            details: {
              code: "doctor-failed",
              message: stderr.trim() || `Doctor reported ${parsed.failed_checks.length} failed check(s)`,
              doctorCommand
            }
          });
          return;
        }
        const aborted = abortReason(parsed);
        resolve(failureResult2(ctx, "doctor-failed", aborted || stderr.trim() || `Doctor command exited with code ${exitCode}`, doctorCommand));
      } else {
        let parsed;
        try {
          parsed = JSON.parse(stdout);
        } catch (err) {
          const message = err instanceof Error ? err.message : "Failed to parse doctor output";
          resolve(failureResult2(ctx, "parse-error", message, doctorCommand));
          return;
        }
        if (!isDoctorResult(parsed)) {
          resolve(failureResult2(ctx, "parse-error", "Doctor output is missing its check lists", doctorCommand));
          return;
        }
        resolve({
          content: [{ type: "text", text: formatDoctorOutput(parsed) }],
          details: parsed
        });
      }
    });
    proc.on("error", (err) => {
      const doctorCommand = `${ctx.crewdPath} doctor --state-dir ${ctx.stateDir} --repo ${ctx.repository}`;
      resolve(failureResult2(ctx, "spawn-error", err.message, doctorCommand));
    });
  });
}
function failureResult2(ctx, code, message, doctorCommand) {
  return {
    isError: true,
    content: [{ type: "text", text: `Doctor command failed: ${message}` }],
    details: {
      code,
      message,
      doctorCommand: doctorCommand ?? `${ctx.crewdPath} doctor --state-dir ${ctx.stateDir} --repo ${ctx.repository}`
    }
  };
}
function isDoctorResult(value) {
  if (value === null || typeof value !== "object")
    return false;
  if (!("passed_checks" in value) || !("failed_checks" in value))
    return false;
  return Array.isArray(value.passed_checks) && Array.isArray(value.failed_checks);
}
function abortReason(value) {
  if (value === null || typeof value !== "object" || !("error" in value))
    return;
  return typeof value.error === "string" ? value.error : undefined;
}
function formatDoctorOutput(result) {
  const lines = [];
  lines.push(`Doctor check: ${result.healthy ? "healthy" : "failed"}`);
  if (result.passed_checks.length > 0) {
    lines.push(`Passed checks: ${result.passed_checks.join(", ")}`);
  }
  if (result.failed_checks.length > 0) {
    lines.push("Failed checks:");
    for (const check of result.failed_checks) {
      lines.push(`  - ${check.check_name}: ${check.error}`);
    }
  }
  if (result.unresolved_gates.length > 0) {
    lines.push(`Unresolved gates: ${result.unresolved_gates.join(", ")}`);
  }
  return lines.join(`
`);
}

// src/install.ts
import { homedir as homedir3 } from "os";

// src/download.ts
import { chmodSync, mkdirSync, renameSync, unlinkSync, writeFileSync } from "fs";
import { join as join4 } from "path";
var API_BASE_URL = "https://api.github.com/repos/nikolasd/batman";

class RuntimeDownloadError extends Error {
  code;
  constructor(code, message) {
    super(message);
    this.name = "RuntimeDownloadError";
    this.code = code;
  }
}
async function downloadRuntime(options) {
  const fetchImpl = options.fetchImpl ?? fetch;
  const apiBaseUrl = options.apiBaseUrl ?? API_BASE_URL;
  const tag = `v${options.version}`;
  const binaryName = `crewd-${options.target}`;
  const manifestName = `${binaryName}.manifest.json`;
  const releaseUrl = `${apiBaseUrl}/releases/tags/${tag}`;
  const assets = await fetchReleaseAssets(fetchImpl, releaseUrl, options.token);
  const manifestAsset = findAsset(assets, manifestName, releaseUrl);
  const binaryAsset = findAsset(assets, binaryName, releaseUrl);
  const manifestRaw = await fetchAssetText(fetchImpl, manifestAsset.url, options.token);
  const manifest = parseManifest(manifestRaw, manifestAsset.url);
  if (manifest.version !== options.version) {
    throw new BinaryIntegrityError("version-mismatch", `manifest at ${manifestAsset.url} declares version ${manifest.version}, but ${options.version} was requested`);
  }
  if (manifest.target !== options.target) {
    throw new BinaryIntegrityError("manifest-invalid", `manifest at ${manifestAsset.url} declares target ${manifest.target}, but ${options.target} was requested`);
  }
  const binaryBytes = await fetchAssetBytes(fetchImpl, binaryAsset.url, options.token);
  const dir = runtimeCacheDir(options.stateRoot, options.version);
  const finalPath = join4(dir, "crewd");
  const manifestPath = join4(dir, "manifest.json");
  const tmpPath = join4(dir, `.crewd.${process.pid}.tmp`);
  try {
    mkdirSync(dir, { recursive: true, mode: 448 });
    writeFileSync(tmpPath, binaryBytes);
    chmodSync(tmpPath, 493);
  } catch (err) {
    throw new RuntimeDownloadError("write-failed", `failed to write ${tmpPath}: ${err.message}`);
  }
  const actualSha256 = sha256File(tmpPath);
  if (actualSha256 !== manifest.sha256) {
    try {
      unlinkSync(tmpPath);
    } catch {}
    throw new BinaryIntegrityError("checksum-mismatch", `checksum mismatch for ${binaryAsset.url}: manifest ${manifestAsset.url} declares ${manifest.sha256}, computed ${actualSha256}`);
  }
  try {
    renameSync(tmpPath, finalPath);
    writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}
`);
  } catch (err) {
    throw new RuntimeDownloadError("write-failed", `failed to finalize ${finalPath}: ${err.message}`);
  }
  return { path: finalPath, version: manifest.version, target: manifest.target, sizeBytes: manifest.sizeBytes };
}
function githubHeaders(token, accept) {
  return token === undefined ? { Accept: accept } : { Accept: accept, Authorization: `Bearer ${token}` };
}
async function fetchReleaseAssets(fetchImpl, releaseUrl, token) {
  const response = await fetchImpl(releaseUrl, { headers: githubHeaders(token, "application/vnd.github+json") });
  if (!response.ok) {
    throw new RuntimeDownloadError("http-error", `failed to fetch release ${releaseUrl}: HTTP ${response.status}`);
  }
  const raw = await response.json();
  const assets = raw.assets;
  if (!Array.isArray(assets)) {
    throw new RuntimeDownloadError("http-error", `release ${releaseUrl} response has no assets array`);
  }
  return assets.map((asset) => ({
    name: String(asset.name ?? ""),
    url: String(asset.url ?? "")
  }));
}
function findAsset(assets, name, releaseUrl) {
  const asset = assets.find((candidate) => candidate.name === name);
  if (asset === undefined) {
    throw new RuntimeDownloadError("http-error", `release ${releaseUrl} has no asset named ${name}`);
  }
  return asset;
}
async function fetchAsset(fetchImpl, assetUrl, token) {
  const response = await fetchImpl(assetUrl, { headers: githubHeaders(token, "application/octet-stream") });
  if (!response.ok) {
    throw new RuntimeDownloadError("http-error", `failed to download ${assetUrl}: HTTP ${response.status}`);
  }
  return response;
}
async function fetchAssetText(fetchImpl, assetUrl, token) {
  return (await fetchAsset(fetchImpl, assetUrl, token)).text();
}
async function fetchAssetBytes(fetchImpl, assetUrl, token) {
  return Buffer.from(await (await fetchAsset(fetchImpl, assetUrl, token)).arrayBuffer());
}

// src/install.ts
function buildRuntimeInstallContext(env, home = homedir3()) {
  return {
    version: package_default.version,
    target: resolveTarget(process.platform, process.arch, detectLibc()),
    stateRoot: resolveStateRoot(env, home),
    token: resolveGitHubToken(env)
  };
}
async function runRuntimeInstall(ctx) {
  try {
    const result = await downloadRuntime({ version: ctx.version, target: ctx.target, stateRoot: ctx.stateRoot, token: ctx.token });
    return {
      content: [{ type: "text", text: `Crew runtime installed: crewd ${result.version} (${result.target})
Path: ${result.path}` }],
      details: { version: result.version, target: result.target, path: result.path, sizeBytes: result.sizeBytes }
    };
  } catch (err) {
    const code = installErrorCode(err);
    const message = err instanceof Error ? err.message : String(err);
    return {
      isError: true,
      content: [{ type: "text", text: `Runtime install failed: ${message}` }],
      details: { code, message }
    };
  }
}
async function installRuntimeForEnv(env, home) {
  let ctx;
  try {
    ctx = buildRuntimeInstallContext(env, home);
  } catch (err) {
    const code = installErrorCode(err);
    const message = err instanceof Error ? err.message : String(err);
    return {
      isError: true,
      content: [{ type: "text", text: `Runtime install failed: ${message}` }],
      details: { code, message }
    };
  }
  return runRuntimeInstall(ctx);
}
function installErrorCode(err) {
  if (err instanceof RuntimeDownloadError || err instanceof BinaryIntegrityError || err instanceof UnsupportedPlatformError) {
    return err.code;
  }
  return "unknown-error";
}
function resolveGitHubToken(env) {
  return env.GITHUB_TOKEN || env.GH_TOKEN || tryGhAuthToken();
}
function tryGhAuthToken() {
  try {
    const result = Bun.spawnSync(["gh", "auth", "token"]);
    if (result.exitCode !== 0) {
      return;
    }
    const token = result.stdout.toString("utf8").trim();
    return token.length > 0 ? token : undefined;
  } catch {
    return;
  }
}

// src/approval-ui.ts
var APPROVAL_DIALOG_TIMEOUT_MS = 5 * 60 * 1000;
var SECRET_KEY_PATTERN = /token|secret|password|apikey|api_key|credential/i;
async function showApprovalDialog(ui, approval) {
  if (!approval.humanRequired) {
    return;
  }
  ui.notify(renderApprovalMessage(approval), "info");
  const selection = await ui.select(`Approval required: ${approval.action}`, ["Approve", "Deny"], {
    timeout: APPROVAL_DIALOG_TIMEOUT_MS
  });
  if (selection === undefined) {
    return;
  }
  const decision = selection === "Approve" ? "approve" : "deny";
  const reason = await ui.input(decision === "approve" ? "Reason for approving" : "Reason for denying", "", { timeout: APPROVAL_DIALOG_TIMEOUT_MS });
  if (reason === undefined) {
    return;
  }
  return { decision, reason };
}
function renderApprovalMessage(approval) {
  const lines = [`Approval ID: ${approval.approvalId}`];
  if (approval.workerId !== undefined) {
    lines.push(`Worker: ${approval.workerId}`);
  }
  lines.push(`Action: ${approval.action}`);
  lines.push(`Arguments: ${JSON.stringify(redactArguments(approval.arguments))}`);
  lines.push(`Policy reason: ${approval.policyReason}`);
  return lines.join(`
`);
}
function redactArguments(value) {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return value;
  }
  const redacted = {};
  for (const [key, entryValue] of Object.entries(value)) {
    redacted[key] = SECRET_KEY_PATTERN.test(key) ? "<redacted>" : entryValue;
  }
  return redacted;
}

// src/tools/shared.ts
async function callOrchestration(client, method, params) {
  try {
    const result = await client.request(method, params);
    return {
      content: [{ type: "text", text: renderSummary(method, result) }],
      details: result
    };
  } catch (err) {
    if (err instanceof JsonRpcRemoteError) {
      const details = {
        code: err.code,
        message: err.message,
        data: err.data
      };
      return {
        content: [{ type: "text", text: `${method} failed: ${err.message}` }],
        details,
        isError: true
      };
    }
    throw err;
  }
}
function renderSummary(method, result) {
  return `${method}: ${JSON.stringify(result)}`;
}

// src/tools/approvals.ts
var CREW_APPROVAL_TOOL_NAME = "crew_approval";
async function findPendingApproval(client, approvalId) {
  const result = await client.request("approval/list", {});
  if (typeof result !== "object" || result === null || !("approvals" in result)) {
    return;
  }
  const approvals = result.approvals;
  if (!Array.isArray(approvals)) {
    return;
  }
  const match = approvals.find((entry) => typeof entry === "object" && entry !== null && entry.approvalId === approvalId);
  if (match === undefined) {
    return;
  }
  return {
    approvalId,
    action: typeof match.action === "string" ? match.action : "",
    arguments: match.arguments,
    policyReason: typeof match.policyReason === "string" ? match.policyReason : "",
    humanRequired: match.humanRequired === true
  };
}
function registerApprovalTool(pi, ctx) {
  const params = pi.zod.object({
    op: pi.zod.enum(["list", "decide"]).describe("Which approval operation to perform."),
    runId: pi.zod.string().optional().describe("Optional run id filter for list."),
    approvalId: pi.zod.string().optional().describe("Required for decide: the approval request id."),
    decision: pi.zod.enum(["approve", "deny"]).optional().describe("Required for decide: approve or deny."),
    reason: pi.zod.string().optional().describe("Required for decide: the reason for this decision.")
  });
  pi.registerTool({
    name: CREW_APPROVAL_TOOL_NAME,
    label: "Crew Approval",
    description: "Use when a worker escalates a decision to human (e.g., for risky operations). The runtime shows a dialog; call this to list pending approvals (with human-in-the-loop flag) or decide with the human's approve/deny decision. The runtime enforces humanRequired flags -- never auto-approve, even for list. Use when a worker pauses execution waiting for human input.",
    parameters: params,
    approval: { tier: "exec", override: true, reason: "Approval decisions are a user-facing safety action." },
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const client = await ctx.getClient(extCtx);
      if (input.op !== "decide") {
        return callOrchestration(client, "approval/list", { runId: input.runId });
      }
      if (input.approvalId === undefined) {
        return callOrchestration(client, "approval/decide", {
          approvalId: input.approvalId,
          decision: input.decision,
          reason: input.reason,
          decidedBy: "model"
        });
      }
      const pending = await findPendingApproval(client, input.approvalId);
      if (pending?.humanRequired === true) {
        if (!extCtx.hasUI) {
          return {
            content: [{ type: "text", text: `Approval ${input.approvalId} requires a human decision and no interactive UI is available; it remains pending.` }],
            details: { approvalId: input.approvalId, outcome: "pending", reason: "humanRequiredWithoutUi" },
            isError: true
          };
        }
        const human = await showApprovalDialog(extCtx.ui, pending);
        if (human === undefined) {
          return {
            content: [{ type: "text", text: `Approval dialog timed out; ${input.approvalId} remains pending.` }],
            details: { approvalId: input.approvalId, outcome: "pending" }
          };
        }
        return callOrchestration(client, "approval/decide", {
          approvalId: input.approvalId,
          decision: human.decision,
          reason: human.reason,
          decidedBy: "human"
        });
      }
      return callOrchestration(client, "approval/decide", {
        approvalId: input.approvalId,
        decision: input.decision,
        reason: input.reason,
        decidedBy: "model"
      });
    }
  });
}

// src/tools/artifacts.ts
var ARTIFACT_KINDS = ["patch", "commitList", "conflictReport", "workspaceManifest"];
var CREW_ARTIFACT_TOOL_NAME = "crew_artifact";
function registerArtifactTool(pi, ctx) {
  const params = pi.zod.object({
    op: pi.zod.enum(["list", "fetch"]).describe("Which artifact operation to perform."),
    kind: pi.zod.enum(ARTIFACT_KINDS).optional().describe("Optional filter for list: only return artifacts of this kind. Omit to list every kind."),
    taskId: pi.zod.string().optional().describe("Optional for list: narrow to artifacts from a specific task. Defaults to all tasks owned by the current session."),
    artifactId: pi.zod.string().optional().describe("Required for fetch: the artifact id to read."),
    offset: pi.zod.number().int().optional().describe("Optional for fetch: byte offset to start from. Defaults to 0."),
    length: pi.zod.number().int().optional().describe("Optional for fetch: how many bytes to read. The runtime caps this; the response's nextOffset says where to resume.")
  });
  pi.registerTool({
    name: CREW_ARTIFACT_TOOL_NAME,
    label: "Crew Artifact",
    description: "Use to read the evidence a worker produced: patches, commit lists, conflict reports, and workspace manifests. Use op: 'list' to see what a run published (optionally filtered by kind), then op: 'fetch' with an artifactId to read its bytes. Fetches are chunked -- the response carries nextOffset, so pass it back as offset to continue reading a large artifact. Artifacts are scoped to runs this session owns; taskId only narrows further within them.",
    parameters: params,
    approval: "read",
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const client = await ctx.getClient(extCtx);
      switch (input.op) {
        case "list":
          return callOrchestration(client, "artifact/list", { kind: input.kind, taskId: input.taskId });
        case "fetch":
          return callOrchestration(client, "artifact/fetch", {
            artifactId: input.artifactId,
            offset: input.offset,
            length: input.length
          });
      }
    }
  });
}

// src/tools/children.ts
var CREW_CHILD_TOOL_NAME = "crew_child";
function registerChildTool(pi, ctx) {
  const params = pi.zod.object({
    op: pi.zod.enum(["list", "decide"]).describe("Which child-request operation to perform."),
    runId: pi.zod.string().optional().describe("Optional filter for list: only return child requests recorded by this run."),
    parentRunId: pi.zod.string().optional().describe("Required for decide: the run whose child request is being decided."),
    decision: pi.zod.enum(["accept", "deny"]).optional().describe("Required for decide: accept provisions the child run, deny refuses it."),
    childTaskId: pi.zod.string().optional().describe("Required when decision is accept: the task the child run executes."),
    childWorkerId: pi.zod.string().optional().describe("Required when decision is accept: the worker the child run executes as."),
    childRunId: pi.zod.string().optional().describe("Required when decision is accept: the run id to provision for the child."),
    reason: pi.zod.string().optional().describe("Required when decision is deny: why the child was refused.")
  });
  pi.registerTool({
    name: CREW_CHILD_TOOL_NAME,
    label: "Crew Child",
    description: "Use to see and decide nested-worker requests: a worker that wants to spawn a child records the intent, and nothing happens until you decide. Use op: 'list' to see pending requests (optionally filtered by runId), then op: 'decide' with parentRunId and decision. Accepting requires childTaskId, childWorkerId, and childRunId; denying requires reason. A request is only an intent -- accepting is what creates the child run.",
    parameters: params,
    approval: (args) => typeof args === "object" && args !== null && ("op" in args) && args.op === "decide" ? "exec" : "read",
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const client = await ctx.getClient(extCtx);
      switch (input.op) {
        case "list":
          return callOrchestration(client, "coordination/child/list", { runId: input.runId });
        case "decide":
          return callOrchestration(client, "coordination/child/decide", {
            parentRunId: input.parentRunId,
            decision: input.decision,
            childTaskId: input.childTaskId,
            childWorkerId: input.childWorkerId,
            childRunId: input.childRunId,
            reason: input.reason
          });
      }
    }
  });
}

// src/tools/messages.ts
var MESSAGE_KINDS = ["assign", "steer", "followUp", "question", "answer", "peerMessage", "approvalDecision", "cancel", "shutdown"];
var CREW_MESSAGE_TOOL_NAME = "crew_message";
function registerMessageTool(pi, ctx) {
  const params = pi.zod.object({
    op: pi.zod.enum(["send", "list"]).describe("Which message operation to perform."),
    runId: pi.zod.string().describe("The run this message belongs to (required for both send and list)."),
    senderWorkerId: pi.zod.string().optional().describe("Required for send: the sending worker id."),
    taskId: pi.zod.string().optional().describe("Required for send: the task this message relates to."),
    kind: pi.zod.enum(MESSAGE_KINDS).optional().describe("Required for send: the coordination message kind."),
    payload: pi.zod.string().optional().describe("Required for send: the message payload."),
    recipientWorkerId: pi.zod.string().optional().describe("Optional recipient worker id for send."),
    replyTo: pi.zod.string().optional().describe("Optional id of a prior message this replies to.")
  });
  pi.registerTool({
    name: CREW_MESSAGE_TOOL_NAME,
    label: "Crew Message",
    description: "Use to communicate between workers during an active multi-worker run, or to review message history. Use op: 'send' to send a message to another worker (requires runId, senderWorkerId, kind, payload), or op: 'list' to list messages for a run. Message kinds: assign, steer, followUp, question, answer, peerMessage, approvalDecision, cancel, shutdown. Use when workers need to coordinate or escalate decisions.",
    parameters: params,
    approval: "write",
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const client = await ctx.getClient(extCtx);
      if (input.op === "send") {
        return callOrchestration(client, "message/send", {
          runId: input.runId,
          senderWorkerId: input.senderWorkerId,
          taskId: input.taskId,
          kind: input.kind,
          payload: input.payload,
          recipientWorkerId: input.recipientWorkerId,
          replyTo: input.replyTo
        });
      }
      return callOrchestration(client, "message/list", { runId: input.runId });
    }
  });
}

// src/tools/profiles.ts
var CREW_PROFILE_TOOL_NAME = "crew_profile";
function registerProfileTool(pi, ctx) {
  const params = pi.zod.object({
    adapter: pi.zod.string().describe("The adapter name this profile launches, e.g. claude, codex, copilot, ompRpc, terminalDegraded."),
    model: pi.zod.string().describe("The model identifier this profile uses."),
    startupOptions: pi.zod.record(pi.zod.string(), pi.zod.unknown()).describe("Adapter-specific startup options, tagged by adapter kind, e.g. { claude: { ... } } or { codex: { ... } }."),
    environmentAllowlist: pi.zod.array(pi.zod.string()).optional().describe("Environment variable names this profile's process is allowed to read."),
    permissionEnvelope: pi.zod.record(pi.zod.string(), pi.zod.unknown()).optional()
  });
  pi.registerTool({
    name: CREW_PROFILE_TOOL_NAME,
    label: "Crew Profile",
    description: "Use to register a reusable worker profile (adapter, model, startup options, environment allowlist) before provisioning workers against it. Call this once per adapter/model combination, then pass the returned profileId to crew_worker { op: 'create', profileId } instead of repeating fingerprint/adapter/model/permissionEnvelope on every worker. Registration is permanent for the lifetime of the runtime's database; there is no update or delete operation, so register a new profile rather than mutating an existing one.",
    parameters: params,
    approval: () => "exec",
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const client = await ctx.getClient(extCtx);
      return callOrchestration(client, "profile/register", {
        adapter: input.adapter,
        model: input.model,
        startupOptions: input.startupOptions,
        environmentAllowlist: input.environmentAllowlist ?? [],
        permissionEnvelope: input.permissionEnvelope ?? {},
        source: "omp"
      });
    }
  });
}

// src/tools/reconcile.ts
var CREW_RECONCILE_TOOL_NAME = "crew_reconcile";
function registerReconcileTool(pi, ctx) {
  const params = pi.zod.object({
    taskId: pi.zod.string().describe("The task id to rebind to this OMP client instance."),
    revision: pi.zod.number().int().nonnegative().describe("The monotonic OMP revision that must match the stored task.")
  });
  pi.registerTool({
    name: CREW_RECONCILE_TOOL_NAME,
    label: "Crew Reconcile",
    description: "Use after a session drop or reconnect when your OMP session was interrupted and you had active tasks. Rebinds task ownership from the prior session to the current one. Requires matching taskId and monotonic revision (the runtime rejects rebinds on revision mismatch to prevent race conditions). Call when your session was interrupted and restarted, you have active tasks from a prior session that need to be reattached, or the runtime reports ownership conflicts.",
    parameters: params,
    approval: "write",
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const client = await ctx.getClient(extCtx);
      return callOrchestration(client, "reconcile/omp", {
        taskId: input.taskId,
        revision: input.revision
      });
    }
  });
}

// src/tools/runs.ts
var CREW_RUN_TOOL_NAME = "crew_run";
function registerRunTool(pi, ctx) {
  const params = pi.zod.object({
    op: pi.zod.enum(["submit", "list", "get", "retry", "cancel", "result"]).describe("Which run operation to perform."),
    prompt: pi.zod.string().optional().describe("Required for submit and retry: the instruction the worker executes. Crew stores no task text, so the task's description must be passed here."),
    taskId: pi.zod.string().optional().describe("Required for submit: the task to execute. Optional filter for list."),
    workerId: pi.zod.string().optional().describe("Required for submit and retry: the worker to execute with."),
    workspaceMode: pi.zod.enum(["shared", "isolated", "copy"]).optional().describe("Optional workspace mode for submit and retry: 'shared' (the repository itself, the default), 'isolated' (a per-run git worktree), or 'copy' (a per-run copy of the repository)."),
    priority: pi.zod.number().int().optional().describe("Optional priority for submit."),
    runId: pi.zod.string().optional().describe("Required for get, cancel, and result: the run id."),
    priorRunId: pi.zod.string().optional().describe("Required for retry: the terminal run id to retry.")
  });
  pi.registerTool({
    name: CREW_RUN_TOOL_NAME,
    label: "Crew Run",
    description: "Use to execute, monitor, or manage task execution by external workers. Use op: 'submit' to start execution (requires taskId from crew_task, workerId from crew_worker, and prompt -- the instruction text the worker executes), op: 'get' to check progress/status of a run, op: 'result' to read a finished run's final output text and token usage (requires runId; refused until the run reaches a terminal state -- chain work by passing resultText into the next submit's prompt), op: 'list' to list runs for a task, op: 'retry' to re-execute a terminal run (creates a new runId and starts a fresh worker process; pass prompt again), or op: 'cancel' to stop a running run. After submitting, monitor with op: 'get'. If the run fails, retry with op: 'retry' (new runId). If stuck, cancel with op: 'cancel'.",
    parameters: params,
    approval: (args) => typeof args === "object" && args !== null && ("op" in args) && (args.op === "submit" || args.op === "retry" || args.op === "cancel") ? "exec" : "read",
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const client = await ctx.getClient(extCtx);
      switch (input.op) {
        case "submit":
          return callOrchestration(client, "run/submit", {
            taskId: input.taskId,
            prompt: input.prompt,
            workerId: input.workerId,
            workspaceMode: input.workspaceMode,
            priority: input.priority
          });
        case "list":
          return callOrchestration(client, "run/list", { taskId: input.taskId });
        case "get":
          return callOrchestration(client, "run/get", { runId: input.runId });
        case "result":
          return callOrchestration(client, "run/result", { runId: input.runId });
        case "retry":
          return callOrchestration(client, "run/retry", {
            priorRunId: input.priorRunId,
            workerId: input.workerId,
            prompt: input.prompt,
            workspaceMode: input.workspaceMode
          });
        case "cancel":
          return callOrchestration(client, "run/cancel", { runId: input.runId });
      }
    }
  });
}

// src/tools/tasks.ts
var CREW_TASK_TOOL_NAME = "crew_task";
var INITIAL_TASK_REVISION = 0;
function registerTaskTool(pi, ctx) {
  const params = pi.zod.object({
    op: pi.zod.enum(["upsert", "get"]).describe("Which task operation to perform."),
    taskId: pi.zod.string().optional().describe("Optional for upsert: reuse an existing task ID (for resume); auto-generated if omitted. Required for get.")
  });
  pi.registerTool({
    name: CREW_TASK_TOOL_NAME,
    label: "Crew Task",
    description: "Use when you need to create a persistent, cross-session unit of work that will be executed by an external AI harness (Claude, Codex, Copilot, or OMP-RPC) -- not OMP's native in-process task subagent. Use op: 'upsert' to create or update a task, or op: 'get' to read one back. Crew stores no task text: the task graph and its descriptions live in OMP, and the instruction a worker executes is passed to crew_run as prompt. Persists across session disconnects (stored in SQLite journal), executes via external harness processes, and can be retried, cancelled, or reconciled after failure. Auto-generates a task ID and uses your OMP session as owner. After creating, select a worker with crew_worker { op: 'list' } and submit execution with crew_run { op: 'submit', taskId, workerId, prompt }.",
    parameters: params,
    approval: (args) => typeof args === "object" && args !== null && ("op" in args) && args.op === "get" ? "read" : "write",
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const client = await ctx.getClient(extCtx);
      switch (input.op) {
        case "upsert": {
          const taskId = input.taskId ?? crypto.randomUUID();
          const result = await callOrchestration(client, "task/upsert", {
            taskId,
            ownerClientInstanceId: extCtx.sessionManager.getSessionId(),
            revision: INITIAL_TASK_REVISION
          });
          if (result.isError !== true) {
            try {
              pi.appendEntry(OMP_NATIVE_CORRELATION_ENTRY_TYPE, {
                taskId,
                revision: INITIAL_TASK_REVISION
              });
            } catch (err) {
              pi.logger.warn("crew task: failed to persist task correlation", {
                taskId,
                error: err instanceof Error ? err.message : String(err)
              });
            }
          }
          return result;
        }
        case "get":
          return callOrchestration(client, "task/get", { taskId: input.taskId });
      }
    }
  });
}

// src/tools/violations.ts
var CREW_VIOLATION_TOOL_NAME = "crew_violation";
function registerViolationTool(pi, ctx) {
  const params = pi.zod.object({
    op: pi.zod.enum(["decide", "list"]).describe("Which violation operation to perform."),
    violationId: pi.zod.string().optional().describe("Required for decide: the recorded violation to decide."),
    resolution: pi.zod.enum(["release", "cancel"]).optional().describe("Required for decide: 'release' resumes the quarantined run (if this was its last unresolved violation), 'cancel' ends the run outright."),
    runId: pi.zod.string().optional().describe("Optional for list: narrow to one run's violations.")
  });
  pi.registerTool({
    name: CREW_VIOLATION_TOOL_NAME,
    label: "Crew Violation",
    description: `Use to find and resolve policy violations. Use op: 'list' (optionally with runId) to see every recorded violation and its decision state -- an entry with resolution: null on a quarantined run is the one holding the quarantine. Use op: 'decide' with the violationId and a resolution to resolve one. The deciding identity is taken from your session automatically. A "release" only lifts quarantine if this was the last unresolved violation on the run -- check the result's quarantineCleared field (true/false/absent) to tell whether it did; if false, use op: 'list' to find the still-open violation. Until every violation on a run is decided, the run makes no further progress.`,
    parameters: params,
    approval: (args) => typeof args === "object" && args !== null && ("op" in args) && args.op === "list" ? "read" : "exec",
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const client = await ctx.getClient(extCtx);
      switch (input.op) {
        case "list":
          return callOrchestration(client, "policy/violation/list", { runId: input.runId });
        case "decide":
          return callOrchestration(client, "policy/violation/decide", {
            violationId: input.violationId,
            resolution: input.resolution
          });
      }
    }
  });
}

// src/tools/workers.ts
var CREW_WORKER_TOOL_NAME = "crew_worker";
function registerWorkerTool(pi, ctx) {
  const params = pi.zod.object({
    op: pi.zod.enum(["create", "list", "get"]).describe("Which worker operation to perform."),
    fingerprint: pi.zod.string().optional().describe("Required for create: a fingerprint of the harness binary + version."),
    adapter: pi.zod.string().optional().describe("Required for create: the adapter name, e.g. claude, codex, copilot, ompNative."),
    model: pi.zod.string().optional().describe("Required for create: the model identifier this worker uses."),
    profileId: pi.zod.string().optional().describe("Optional profile id for the worker identity."),
    permissionEnvelope: pi.zod.record(pi.zod.string(), pi.zod.unknown()).optional(),
    parentWorkerId: pi.zod.string().optional().describe("Parent worker id, if spawned as a child."),
    workerId: pi.zod.string().optional().describe("Required for get: the worker id to fetch.")
  });
  pi.registerTool({
    name: CREW_WORKER_TOOL_NAME,
    label: "Crew Worker",
    description: "Use to find or provision external AI harness workers (Claude, Codex, Copilot, OMP-RPC) that execute tasks. Use op: 'list' to see available workers for a repository (call before submitting a run), op: 'get' to fetch details of a specific worker, or op: 'create' to provision a new worker identity for a specific harness/model combination (requires fingerprint, adapter, model). You need a workerId from crew_worker { op: 'list' } to submit a run with crew_run { op: 'submit' }.",
    parameters: params,
    approval: (args) => typeof args === "object" && args !== null && ("op" in args) && args.op === "create" ? "exec" : "read",
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const client = await ctx.getClient(extCtx);
      switch (input.op) {
        case "create":
          return callOrchestration(client, "worker/create", {
            fingerprint: input.fingerprint,
            adapter: input.adapter,
            model: input.model,
            profileId: input.profileId,
            permissionEnvelope: input.permissionEnvelope,
            parentWorkerId: input.parentWorkerId
          });
        case "list":
          return callOrchestration(client, "worker/list", {});
        case "get":
          return callOrchestration(client, "worker/get", { workerId: input.workerId });
      }
    }
  });
}

// src/tools/workspaces.ts
var LEASE_MODES = ["readOnly", "write"];
var ISOLATION_KINDS = ["shared", "gitWorktree", "copy"];
var APPLY_STRATEGIES = ["applyPatch", "cherryPick"];
var CREW_WORKSPACE_TOOL_NAME = "crew_workspace";
function registerWorkspaceTool(pi, ctx) {
  const params = pi.zod.object({
    op: pi.zod.enum(["acquire", "get", "release", "inspect", "apply"]).describe("Which workspace operation to perform."),
    runId: pi.zod.string().optional().describe("Required for acquire: the run this workspace lease belongs to."),
    mode: pi.zod.enum(LEASE_MODES).optional().describe("Required for acquire: readOnly allows sharing with other readers, write requires isolation."),
    requestedIsolation: pi.zod.enum(ISOLATION_KINDS).optional().describe("Optional for acquire: the isolation strategy to materialize. Defaults to shared. Use gitWorktree or copy when a peer agent will work on the same task concurrently."),
    leaseId: pi.zod.string().optional().describe("Required for get, release, inspect, and apply: the lease id."),
    strategy: pi.zod.enum(APPLY_STRATEGIES).optional().describe("Required for apply: applyPatch applies a patch artifact, cherryPick replays commits."),
    artifactId: pi.zod.string().optional().describe("Required for apply: the artifact to apply (from crew_artifact { op: 'list' })."),
    expectedTargetRevision: pi.zod.string().optional().describe("Required for apply: the revision the workspace must currently be at. A mismatch is refused as STALE_REVISION rather than applied to the wrong base."),
    approvalCorrelationId: pi.zod.string().optional().describe("Optional for apply: correlates this application with an approval decision.")
  });
  pi.registerTool({
    name: CREW_WORKSPACE_TOOL_NAME,
    label: "Crew Workspace",
    description: "Use to acquire, inspect, apply changes to, or release an isolated (or shared) working directory for a run. Use op: 'acquire' before submitting a run that needs its own git worktree or copy (requires runId and mode; pass requestedIsolation: 'gitWorktree' for concurrent agents working on the same task in isolation), op: 'get' to fetch a lease's current path and state, op: 'inspect' to read the workspace's dirty/untracked file counts and diverged commits, op: 'apply' to land a patch or cherry-pick an artifact into the workspace (requires strategy, artifactId, and expectedTargetRevision), or op: 'release' to tear down the lease once the run is done with it. A shared-mode write lease is exclusive across the whole project; isolated (gitWorktree or copy) leases never conflict with each other or with shared leases.",
    parameters: params,
    approval: (args) => typeof args === "object" && args !== null && ("op" in args) && (args.op === "acquire" || args.op === "release" || args.op === "apply") ? "exec" : "read",
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const client = await ctx.getClient(extCtx);
      switch (input.op) {
        case "acquire":
          return callOrchestration(client, "workspace/acquire", {
            runId: input.runId,
            mode: input.mode,
            requestedIsolation: input.requestedIsolation
          });
        case "get":
          return callOrchestration(client, "workspace/get", { leaseId: input.leaseId });
        case "release":
          return callOrchestration(client, "workspace/release", { leaseId: input.leaseId });
        case "inspect":
          return callOrchestration(client, "workspace/inspect", { leaseId: input.leaseId });
        case "apply":
          return callOrchestration(client, "workspace/apply", {
            leaseId: input.leaseId,
            strategy: input.strategy,
            artifactId: input.artifactId,
            expectedTargetRevision: input.expectedTargetRevision,
            approvalCorrelationId: input.approvalCorrelationId
          });
      }
    }
  });
}

// src/tools/index.ts
function registerOrchestrationTools(pi, ctx) {
  registerTaskTool(pi, ctx);
  registerWorkerTool(pi, ctx);
  registerProfileTool(pi, ctx);
  registerRunTool(pi, ctx);
  registerWorkspaceTool(pi, ctx);
  registerArtifactTool(pi, ctx);
  registerChildTool(pi, ctx);
  registerViolationTool(pi, ctx);
  registerMessageTool(pi, ctx);
  registerApprovalTool(pi, ctx);
  registerReconcileTool(pi, ctx);
}

// src/monitor/model.ts
var EMPTY_FLAGS = {
  degradedControl: false,
  needsReconciliation: false,
  protocolUnhealthy: false,
  policyQuarantined: false,
  workspaceDirty: false,
  childrenActive: false
};
var EMPTY_MONITOR_STATE = { rows: {}, lastSequence: 0 };
function hasVisibleRows(state) {
  return Object.keys(state.rows).length > 0;
}
function reduceEvent(state, envelope) {
  const lastSequence = envelope.sequence > state.lastSequence ? envelope.sequence : state.lastSequence;
  const patch = eventPatch(envelope);
  if (patch === undefined) {
    return { rows: state.rows, lastSequence };
  }
  const existing = state.rows[patch.runId];
  if (existing !== undefined && envelope.sequence <= existing.lastAppliedSequence) {
    return { rows: state.rows, lastSequence };
  }
  const base = existing ?? {
    runId: patch.runId,
    taskId: patch.taskId ?? "",
    workerId: patch.workerId ?? "",
    state: "queued",
    flags: EMPTY_FLAGS,
    pendingApprovalCount: 0,
    openViolations: {},
    firstSeenAt: envelope.timestamp,
    lastEventAt: envelope.timestamp,
    lastAppliedSequence: envelope.sequence
  };
  const updated = {
    ...base,
    taskId: patch.taskId ?? base.taskId,
    workerId: patch.workerId ?? base.workerId,
    state: patch.state ?? base.state,
    flags: patch.flags ?? base.flags,
    latestActivity: patch.latestActivity ?? base.latestActivity,
    pendingApprovalCount: patch.pendingApprovalCountDelta !== undefined ? Math.max(0, base.pendingApprovalCount + patch.pendingApprovalCountDelta) : base.pendingApprovalCount,
    openViolations: applyViolationPatch(base.openViolations, patch),
    lastEventAt: envelope.timestamp,
    lastAppliedSequence: envelope.sequence
  };
  return {
    rows: { ...state.rows, [patch.runId]: updated },
    lastSequence
  };
}
function applyViolationPatch(open, patch) {
  if (patch.addViolation !== undefined) {
    return { ...open, [patch.addViolation.violationId]: patch.addViolation.code };
  }
  if (patch.removeViolationId !== undefined && patch.removeViolationId in open) {
    const next = { ...open };
    delete next[patch.removeViolationId];
    return next;
  }
  return open;
}
function eventPatch(envelope) {
  const event = envelope.event;
  const runId = envelope.runId ?? undefined;
  switch (event.type) {
    case "runEvent": {
      return {
        runId: event.payload.runId,
        taskId: event.payload.taskId,
        workerId: event.payload.workerId,
        state: event.payload.state,
        latestActivity: `run ${event.payload.state}`
      };
    }
    case "runFlagsEvent": {
      return {
        runId: event.payload.runId,
        flags: {
          degradedControl: event.payload.flags.degradedControl,
          needsReconciliation: event.payload.flags.needsReconciliation,
          protocolUnhealthy: event.payload.flags.protocolUnhealthy,
          policyQuarantined: event.payload.flags.policyQuarantined,
          workspaceDirty: event.payload.flags.workspaceDirty,
          childrenActive: event.payload.flags.childrenActive
        }
      };
    }
    case "messageEvent": {
      if (runId === null || runId === undefined) {
        return;
      }
      return {
        runId,
        taskId: event.payload.taskId,
        latestActivity: `${event.payload.kind} ${event.payload.deliveryState}`
      };
    }
    case "approvalEvent": {
      if (runId === null || runId === undefined) {
        return;
      }
      const isRequest = event.payload.kind === "approvalRequested";
      return {
        runId,
        taskId: event.payload.taskId,
        latestActivity: isRequest ? `approval requested: ${event.payload.action}` : `approval decided${event.payload.reason !== undefined && event.payload.reason !== null ? `: ${event.payload.reason}` : ""}`,
        pendingApprovalCountDelta: isRequest ? 1 : -1
      };
    }
    case "childEvent": {
      if (runId === null || runId === undefined) {
        return;
      }
      const label = event.payload.kind === "childWorkerRequested" ? "child worker requested" : event.payload.kind === "childWorkerAccepted" ? "child worker accepted" : "child worker request denied";
      return { runId, latestActivity: label };
    }
    case "policyViolationRecorded": {
      const kind = event.payload.kind;
      if (typeof kind !== "object" || !("policyViolationRecorded" in kind)) {
        return;
      }
      const recorded = kind.policyViolationRecorded;
      return {
        runId: event.payload.runId,
        taskId: event.payload.taskId,
        workerId: event.payload.workerId,
        latestActivity: `policy violation: ${recorded.code}`,
        addViolation: { violationId: recorded.violation_id, code: recorded.code }
      };
    }
    case "policyViolationDecided": {
      const kind = event.payload.kind;
      if (typeof kind !== "object" || !("policyViolationDecided" in kind)) {
        return;
      }
      const decided = kind.policyViolationDecided;
      return {
        runId: event.payload.runId,
        latestActivity: `violation decided: ${decided.resolution}`,
        removeViolationId: decided.violation_id
      };
    }
    case "adapterUsageEvent": {
      const { inputTokens, outputTokens, costUsd } = event.payload;
      const cost = costUsd === null || costUsd === undefined ? "" : ` ($${costUsd})`;
      return {
        runId: event.payload.runId,
        taskId: event.payload.taskId,
        latestActivity: `usage ${inputTokens} in / ${outputTokens} out${cost}`
      };
    }
    case "adapterProtocolHealthEvent": {
      const { healthy, detail } = event.payload;
      const label = healthy ? "protocol healthy" : "protocol unhealthy";
      return {
        runId: event.payload.runId,
        taskId: event.payload.taskId,
        workerId: event.payload.workerId,
        latestActivity: detail === null || detail === undefined ? label : `${label}: ${detail}`
      };
    }
    case "adapterArtifactEvent": {
      return {
        runId: event.payload.runId,
        taskId: event.payload.taskId,
        latestActivity: `artifact ${event.payload.artifactKind} ${event.payload.artifactId}`
      };
    }
    case "workspaceEvent": {
      const kindLabel = event.payload.kind.type;
      return {
        runId: event.payload.runId,
        latestActivity: `workspace ${kindLabel}`
      };
    }
    default:
      return;
  }
}

// src/monitor/render.ts
var MAX_WIDGET_ROWS = 7;
function codePointLength(text) {
  return Array.from(text).length;
}
var BAT_ICON = "\uDB82\uDF5F";
var WIDGET_HEADER_TEXT = "Crew";
var STATE_ICONS = {
  queued: "\uDB80\uDD50",
  starting: "\uDB85\uDCDF",
  working: "\uDB85\uDC61",
  waitingUser: "\uDB82\uDF5A",
  waitingPeer: "\uDB80\uDC0F",
  paused: "\uDB80\uDFE6",
  succeeded: "\uDB81\uDDE1",
  failed: "\uDB80\uDD5A",
  cancelled: "\uDB81\uDF3A",
  lost: "\uDB82\uDFA6"
};
var FALLBACK_STATE_ICON = "\uDB81\uDE25";
function stateIcon(state) {
  return STATE_ICONS[state] ?? FALLBACK_STATE_ICON;
}
var STATE_COLORS = {
  queued: "muted",
  starting: "accent",
  working: "accent",
  waitingUser: "warning",
  waitingPeer: "warning",
  paused: "muted",
  succeeded: "success",
  failed: "error",
  cancelled: "dim",
  lost: "error"
};
var FALLBACK_STATE_COLOR = "text";
function stateColor(state) {
  return STATE_COLORS[state] ?? FALLBACK_STATE_COLOR;
}
function renderWidgetHeader() {
  return `${BAT_ICON} ${WIDGET_HEADER_TEXT}`;
}
function selectRows(state) {
  const rows = Object.values(state.rows).sort((a, b) => a.lastEventAt < b.lastEventAt ? 1 : -1);
  return { rows: rows.slice(0, MAX_WIDGET_ROWS), totalCount: rows.length };
}
function renderRowLine(row) {
  const parts = [shortId(row.runId), `${stateIcon(row.state)} ${row.state}`];
  const harness = harnessLabel(row);
  if (harness !== undefined) {
    parts.push(harness);
  }
  const flags = activeFlagLabels(row.flags);
  if (flags.length > 0) {
    parts.push(`[${flags.join(",")}]`);
  }
  if (row.pendingApprovalCount > 0) {
    parts.push(`${row.pendingApprovalCount} pending approval${row.pendingApprovalCount === 1 ? "" : "s"}`);
  }
  if (row.workspaceMode !== undefined) {
    parts.push(row.workspaceMode);
  }
  if (row.latestActivity !== undefined) {
    parts.push(row.latestActivity);
  }
  return parts.join(" \xB7 ");
}
function assembleBox(header, lines, colors, theme) {
  const { topLeft, topRight, bottomLeft, bottomRight, horizontal, vertical } = theme.boxRound;
  const contentWidth = Math.max(...lines.map((line) => codePointLength(line))) + 2;
  const width = Math.max(contentWidth, codePointLength(header) + 4);
  const top = theme.fg("border", `${topLeft}${horizontal} `) + theme.fg("accent", header) + theme.fg("border", ` ${horizontal.repeat(width - codePointLength(header) - 3)}${topRight}`);
  const body = lines.map((line, index) => {
    const pad = width - codePointLength(line) - 1;
    return theme.fg("border", vertical) + " " + theme.fg(colors[index] ?? "text", line) + " ".repeat(pad) + theme.fg("border", vertical);
  });
  const bottom = theme.fg("border", `${bottomLeft}${horizontal.repeat(width)}${bottomRight}`);
  return [top, ...body, bottom];
}
function renderWidgetBox(state, theme) {
  const { rows, totalCount } = selectRows(state);
  let lines;
  let colors;
  if (totalCount === 0) {
    lines = ["No Crew runs yet."];
    colors = ["text"];
  } else {
    lines = rows.map(renderRowLine);
    colors = rows.map((row) => stateColor(row.state));
    if (totalCount > MAX_WIDGET_ROWS) {
      lines.push(`\u2026 ${totalCount - MAX_WIDGET_ROWS} more; use /crew status <runId> for full details.`);
      colors.push("muted");
    }
  }
  return assembleBox(renderWidgetHeader(), lines, colors, theme);
}
function renderRowDetails(row) {
  const lines = [`Run: ${row.runId}`, `Task: ${row.taskId}`, `Worker: ${row.workerId}`, `State: ${row.state}`];
  const harness = harnessLabel(row);
  if (harness !== undefined) {
    lines.push(`Harness/model: ${harness}`);
  }
  const flags = activeFlagLabels(row.flags);
  lines.push(`Flags: ${flags.length > 0 ? flags.join(", ") : "none"}`);
  if (row.flags.childrenActive) {
    lines.push("Children: active -- list and decide with crew_child");
  }
  const openViolations = Object.entries(row.openViolations);
  if (openViolations.length > 0) {
    lines.push(`Open violations: ${openViolations.map(([id, code]) => `${code} (${id})`).join(", ")} -- decide with crew_violation`);
  }
  lines.push(`Pending approvals: ${row.pendingApprovalCount}`);
  if (row.workspaceMode !== undefined) {
    lines.push(`Workspace mode: ${row.workspaceMode}`);
  }
  if (row.latestActivity !== undefined) {
    lines.push(`Latest activity: ${row.latestActivity}`);
  }
  lines.push(`First seen: ${row.firstSeenAt}`);
  lines.push(`Last event: ${row.lastEventAt}`);
  return lines.join(`
`);
}
function harnessLabel(row) {
  if (row.adapter === undefined) {
    return;
  }
  return row.model === undefined ? row.adapter : `${row.adapter}/${row.model}`;
}
function activeFlagLabels(flags) {
  const labels = [];
  if (flags.degradedControl)
    labels.push("degraded");
  if (flags.needsReconciliation)
    labels.push("needsReconciliation");
  if (flags.protocolUnhealthy)
    labels.push("protocolUnhealthy");
  if (flags.policyQuarantined)
    labels.push("policyQuarantined");
  if (flags.workspaceDirty)
    labels.push("workspaceDirty");
  if (flags.childrenActive)
    labels.push("childrenActive");
  return labels;
}
function shortId(id) {
  return id.length > 8 ? id.slice(0, 8) : id;
}

// src/monitor/controller.ts
var MONITOR_ENTRY_TYPE = "crew-monitor";
var WIDGET_KEY = "crew-monitor";
var MONITOR_COMMAND_NAME = "crew";
function lastPersistedSequence(entries) {
  for (let i = entries.length - 1;i >= 0; i--) {
    const entry = entries[i];
    if (entry?.type === "custom" && entry.customType === MONITOR_ENTRY_TYPE) {
      const data = entry.data;
      if (typeof data?.sequence === "number") {
        return data.sequence;
      }
    }
  }
  return 0;
}

class MonitorController {
  #state = EMPTY_MONITOR_STATE;
  #unsubscribe;
  #onUpdate;
  getState() {
    return this.#state;
  }
  start(client, fromSequence, onUpdate) {
    this.#onUpdate = onUpdate;
    this.#unsubscribe = client.subscribe(fromSequence, (event) => {
      this.#state = reduceEvent(this.#state, event);
      this.#onUpdate?.();
    });
  }
  stop() {
    this.#unsubscribe?.();
    this.#unsubscribe = undefined;
    this.#onUpdate = undefined;
  }
  renderStatus(runId) {
    const row = this.#state.rows[runId];
    return row === undefined ? undefined : renderRowDetails(row);
  }
}
function registerMonitor(pi, ctx) {
  const controller = new MonitorController;
  let subscribedClient;
  function refresh(extCtx, force = false) {
    const state = controller.getState();
    const content = force || hasVisibleRows(state) ? renderWidgetBox(state, extCtx.ui.theme) : undefined;
    extCtx.ui.setWidget(WIDGET_KEY, content, { placement: "aboveEditor" });
    pi.appendEntry(MONITOR_ENTRY_TYPE, { sequence: Number(state.lastSequence) });
  }
  async function connect(extCtx) {
    if (subscribedClient !== undefined && !subscribedClient.isClosed) {
      return;
    }
    if (subscribedClient !== undefined) {
      controller.stop();
      subscribedClient = undefined;
    }
    const fromSequence = Math.max(lastPersistedSequence(extCtx.sessionManager.getEntries()), Number(controller.getState().lastSequence));
    try {
      const client = await ctx.getClient(extCtx);
      controller.start(client, fromSequence, () => refresh(extCtx));
      subscribedClient = client;
    } catch (err) {
      pi.logger.warn("crew monitor: runtime unavailable", {
        error: err instanceof Error ? err.message : String(err)
      });
    }
  }
  pi.on("session_start", async (_event, extCtx) => {
    await connect(extCtx);
    if (subscribedClient !== undefined) {
      refresh(extCtx);
    }
  });
  pi.registerCommand(MONITOR_COMMAND_NAME, {
    description: "Opens or refreshes the embedded Crew worker monitor. `/crew status <runId>` shows full details.",
    handler: async (args, cmdCtx) => {
      const [sub, runId] = args.trim().split(/\s+/, 2);
      if (sub === "status" && runId !== undefined && runId.length > 0) {
        const details = controller.renderStatus(runId);
        cmdCtx.ui.notify(details ?? `No Crew run found for ${runId}.`, details === undefined ? "warning" : "info");
        return;
      }
      await connect(cmdCtx);
      refresh(cmdCtx, true);
    }
  });
  pi.on("session_shutdown", async () => {
    controller.stop();
    subscribedClient = undefined;
  });
}

// src/index.ts
var TOOL_NAME = "crew_health";
var COMMAND_NAME = "crew-status";
var STATUS_DESCRIPTION = "Use to verify the Crew runtime is reachable and healthy before orchestration operations. Returns connection status, runtime identity, and binary source. Call this if you're unsure the daemon is running, or after a connection failure.";
var RUNTIME_INSTALL_TOOL_NAME = "crew_runtime_install";
function crewExtension(pi) {
  let cachedClient;
  function statusContextFor(extCtx) {
    const { ensureRuntimeOptions } = buildStatusContext({ cwd: extCtx.cwd, sessionId: extCtx.sessionManager.getSessionId() });
    return {
      ensureRuntimeOptions,
      cache: {
        get: () => cachedClient,
        set: (client) => {
          cachedClient = client;
        }
      }
    };
  }
  async function getClient(extCtx) {
    return resolveClient(statusContextFor(extCtx));
  }
  pi.registerTool({
    name: TOOL_NAME,
    label: "Crew Health",
    description: STATUS_DESCRIPTION,
    parameters: pi.zod.object({}),
    async execute(_toolCallId, _params, _signal, _onUpdate, extCtx) {
      return getRuntimeStatus(statusContextFor(extCtx));
    }
  });
  pi.registerCommand(COMMAND_NAME, {
    description: STATUS_DESCRIPTION,
    handler: async (_args, extCtx) => {
      const result = await getRuntimeStatus(statusContextFor(extCtx));
      const text = result.content.map((block) => block.text).join(`
`);
      if (!extCtx.hasUI) {
        console.log(text);
      } else {
        extCtx.ui.notify(text, result.isError ? "error" : "info");
      }
    }
  });
  registerOrchestrationTools(pi, { getClient });
  registerMonitor(pi, { getClient });
  function doctorContextFor(cwd) {
    return buildDoctorContext(cwd);
  }
  pi.registerTool({
    name: "crew_doctor",
    label: "Crew Doctor",
    description: "Use for deep diagnostics when crew_health fails or the runtime is unreachable. Runs checks without connecting to a running daemon -- verifies database, state directory, rollout gates, and configuration. Use when the runtime won't start or status reports errors.",
    parameters: pi.zod.object({}),
    async execute(_toolCallId, _params, _signal, _onUpdate, ctx) {
      return runDoctorCommand(doctorContextFor(ctx.cwd));
    }
  });
  pi.registerCommand("crew-doctor", {
    description: "Run diagnostic checks on the Crew runtime state and configuration.",
    handler: async (_args, ctx) => {
      const result = await runDoctorCommand(doctorContextFor(ctx.cwd));
      const text = result.content.map((block) => block.text).join(`
`);
      if (!ctx.hasUI) {
        console.log(text);
      } else {
        ctx.ui.notify(text, result.isError ? "error" : "info");
      }
    }
  });
  pi.registerTool({
    name: RUNTIME_INSTALL_TOOL_NAME,
    label: "Crew Runtime Install",
    description: "Use to download and verify the crewd runtime binary for this platform. Call this when crew_health or any orchestration tool fails with code 'runtime-not-installed'. Downloads the GitHub release asset matching this extension's version, verifies its SHA-256 against the published manifest, and caches it under the Crew state root. nikolasd/batman is a private repository, so this needs read access to it: set GITHUB_TOKEN or GH_TOKEN, or run `gh auth login` locally.",
    parameters: pi.zod.object({}),
    approval: "exec",
    async execute(_toolCallId, _params, _signal, _onUpdate) {
      return installRuntimeForEnv(process.env);
    }
  });
  pi.registerCommand("crew-runtime-install", {
    description: "Download and verify the crewd runtime binary for this platform.",
    handler: async (_args, ctx) => {
      const result = await installRuntimeForEnv(process.env);
      const text = result.content.map((block) => block.text).join(`
`);
      if (!ctx.hasUI) {
        console.log(text);
      } else {
        ctx.ui.notify(text, result.isError ? "error" : "info");
      }
    }
  });
  const ompProcessEpoch = createOmpProcessEpoch();
  const reconciler = new OmpNativeReconciler((fact) => {
    try {
      pi.appendEntry(OMP_NATIVE_FACT_ENTRY_TYPE, { ...fact });
    } catch (err) {
      pi.logger.warn("crew omp-native: failed to persist fact", {
        error: err instanceof Error ? err.message : String(err)
      });
    }
  });
  let unsubscribers = [];
  pi.on("session_start", async (_payload, extCtx) => {
    unsubscribers = [
      pi.events.on(TASK_SUBAGENT_LIFECYCLE_CHANNEL, (data) => {
        const payload = data;
        reconciler.record(normalizeLifecyclePayload(payload, ompProcessEpoch, Date.now()));
      }),
      pi.events.on(TASK_SUBAGENT_PROGRESS_CHANNEL, (data) => {
        const payload = data;
        reconciler.record(normalizeProgressPayload(payload, ompProcessEpoch, Date.now()));
      }),
      pi.events.on(TASK_SUBAGENT_EVENT_CHANNEL, (data) => {
        const payload = data;
        const fact = normalizeEventPayload(payload);
        if (fact !== undefined) {
          reconciler.record(fact);
        }
      })
    ];
    await reconcilePriorProcess(extCtx);
  });
  async function reconcilePriorProcess(extCtx) {
    const entries = extCtx.sessionManager.getEntries();
    const settled = reconcileAcrossRestart(persistedFacts(entries), ompProcessEpoch);
    for (const fact of settled) {
      if (fact.ompProcessEpoch === ompProcessEpoch && fact.status === "lost") {
        reconciler.record(fact);
      }
    }
    const correlations = persistedCorrelations(entries);
    if (correlations.length === 0) {
      return;
    }
    try {
      const client = await getClient(extCtx);
      for (const correlation of correlations) {
        try {
          await reconcileWithRuntime(client, correlation);
        } catch (err) {
          pi.logger.warn("crew omp-native: task reconciliation refused", {
            taskId: correlation.taskId,
            error: err instanceof Error ? err.message : String(err)
          });
        }
      }
    } catch (err) {
      pi.logger.warn("crew omp-native: runtime unavailable for reconciliation", {
        error: err instanceof Error ? err.message : String(err)
      });
    }
  }
  pi.on("session_shutdown", async () => {
    cachedClient?.close();
    cachedClient = undefined;
    for (const unsubscribe of unsubscribers) {
      unsubscribe();
    }
    unsubscribers = [];
    reconciler.dispose();
  });
}
export {
  crewExtension as default
};
