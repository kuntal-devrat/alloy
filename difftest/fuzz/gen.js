#!/usr/bin/env node
// Fuzz differential generator for the alloy engine.
//
// Randomly composes expressions over the full operator set — arithmetic
// (+ - * / %), bitwise (& | ^ ~), shifts (<< >> >>>), exponentiation (**),
// equality/relational (== != === !== < > <= >=), logical (&& ||), unary
// (- ! ~), ternary, comma, and grouping parens — with correct JS
// parenthesization, then evaluates each expression in Node and filters out
// display-fragile results. V8 and Rust's number printer disagree on values
// with |v| >= 1e18 (V8 switches to exponent notation), tiny non-zero values
// (V8 uses "1e-7" style, Rust prints full decimals), and -0 (V8 prints
// "-0"), so only display-safe cases reach the differential diff. A fixed
// set of known edge cases ("torture") is always included.
//
// Usage: node difftest/fuzz/gen.js <count> <seed> <out-prefix>
//   writes <out-prefix>.js  (one `print(<expr>);` per line, plus the a/b
//                            seed variables)
//          <out-prefix>.txt (one raw expression per line, aligned)

const count = parseInt(process.argv[2] || '1500', 10);
const seed = parseInt(process.argv[3] || '42', 10);
const prefix = process.argv[4] || __dirname + '/cases';

// ---- deterministic PRNG (mulberry32) ------------------------------------
let s = seed >>> 0;
function rnd() {
  s |= 0; s = (s + 0x6D2B79F5) | 0;
  let t = Math.imul(s ^ (s >>> 15), 1 | s);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
const ri = (lo, hi) => lo + Math.floor(rnd() * (hi - lo + 1));
const pick = (arr) => arr[Math.floor(rnd() * arr.length)];

// ---- expression generation ----------------------------------------------
// Every node is { s: string, prec: number, unary: bool }:
//   prec  — top-level operator precedence (13 = atom/parenthesized primary,
//           12 = unary prefix, 11 = **, ... 1 = ||/ternary, 0 = comma)
//   unary — true for prefix -/!/~ (a bare unary cannot be the left operand
//           of **, so ** wraps it in parens)
const BINOPS = [
  { op: '||', p: 1 }, { op: '&&', p: 2 },
  { op: '|', p: 3 }, { op: '^', p: 4 }, { op: '&', p: 5 },
  { op: '==', p: 6 }, { op: '!=', p: 6 }, { op: '===', p: 6 }, { op: '!==', p: 6 },
  { op: '<', p: 7 }, { op: '>', p: 7 }, { op: '<=', p: 7 }, { op: '>=', p: 7 },
  { op: '<<', p: 8 }, { op: '>>', p: 8 }, { op: '>>>', p: 8 },
  { op: '+', p: 9 }, { op: '-', p: 9 },
  { op: '*', p: 10 }, { op: '/', p: 10 }, { op: '%', p: 10 },
  { op: '**', p: 11, right: true },
];

// Atoms are deliberately non-negative so a bare negative literal can never
// become the left operand of ** (`-13 ** 2` is a SyntaxError in JS); the
// only negatives come from the parenthesizing unary construct.
function atom() {
  const r = rnd();
  if (r < 0.07) return { s: 'a', prec: 13, unary: false };
  if (r < 0.12) return { s: 'b', prec: 13, unary: false };
  if (r < 0.16) return { s: pick(['null', 'undefined', 'true', 'false']), prec: 13, unary: false };
  if (r < 0.22) return { s: JSON.stringify(pick(['1', '2', '5', '0', 'abc', '3.5', ''])), prec: 13, unary: false };
  if (r < 0.75) return { s: String(ri(0, 20)), prec: 13, unary: false };
  return { s: String(ri(0, 40) / 2), prec: 13, unary: false }; // 0..20 in 0.5 steps
}

function gen(depth) {
  if (depth <= 0 || rnd() < 0.16) return atom();
  const r = rnd();
  if (r < 0.3) return binop(depth);
  if (r < 0.45) return unaryNode(depth);
  if (r < 0.6) return ternaryNode(depth);
  if (r < 0.68) return commaGroup(depth);
  if (r < 0.78) return chainNode(depth);
  // parenthesized deep subtree — tests grouping parens themselves
  return { s: '(' + gen(depth - 1).s + ')', prec: 13, unary: false };
}

function binop(depth) {
  const op = pick(BINOPS);
  const l = gen(depth - 1);
  const r = gen(depth - 1);
  let ls = l.s, rs = r.s;
  if (op.right) {
    // ** : the left operand must be a primary expression — parenthesize
    // unless it already is one (atoms and groups). The right may be a bare
    // unary or another ** (`2 ** -3`, `2 ** 3 ** 2`).
    if (l.prec < 13 || l.unary) ls = '(' + l.s + ')';
    if (r.prec < op.p) rs = '(' + r.s + ')';
  } else {
    // left-assoc: left binds >= p, right binds > p; parens when not
    if (l.prec < op.p) ls = '(' + l.s + ')';
    if (r.prec <= op.p) rs = '(' + r.s + ')';
  }
  return { s: ls + ' ' + op.op + ' ' + rs, prec: op.p, unary: false };
}

// prefix -/!/~ always groups its operand, so the result is never ambiguous
// and can be used anywhere a unary expression is allowed.
function unaryNode(depth) {
  const op = pick(['-', '!', '~']);
  const e = gen(depth - 1);
  return { s: op + '(' + e.s + ')', prec: 12, unary: true };
}

function ternaryNode(depth) {
  const c = gen(depth - 1);
  const t = gen(depth - 1);
  const f = gen(depth - 1);
  const cs = c.prec < 2 ? '(' + c.s + ')' : c.s;   // cond is a ShortCircuitExpression
  const ts = t.prec < 1 ? '(' + t.s + ')' : t.s;   // branches are AssignmentExpressions
  const fs = f.prec < 1 ? '(' + f.s + ')' : f.s;
  return { s: cs + ' ? ' + ts + ' : ' + fs, prec: 1, unary: false };
}

// The comma operator, always parenthesized so it stays one expression.
function commaGroup(depth) {
  const n = 2 + Math.floor(rnd() * 2);
  const parts = [];
  for (let i = 0; i < n; i++) parts.push(gen(depth - 1).s);
  return { s: '(' + parts.join(', ') + ')', prec: 13, unary: false };
}

// An unparenthesized left-assoc chain (`a - b - c`, `x << y << z`) — tests
// the engine's left-associativity directly.
function chainNode(depth) {
  const op = pick(BINOPS.filter((b) => !b.right));
  const n = 2 + Math.floor(rnd() * 2);
  const parts = [];
  for (let i = 0; i < n; i++) {
    let part = gen(depth - 1);
    if (part.prec <= op.p) part = { s: '(' + part.s + ')', prec: 13, unary: false };
    parts.push(part.s);
  }
  return { s: parts.join(' ' + op.op + ' '), prec: op.p, unary: false };
}

// ---- torture cases: fixed edge cases, always included --------------------
const TORTURE = [
  '0 ** 0', '0 ** 1', '1 ** 0', '2 ** 10', '3 ** 4', '9 ** 9',
  '(-2) ** 3', '(-2) ** 2', '2 ** -1', '2 ** -2', '2 ** 3 ** 2',
  '2147483648 | 0', '2147483647 | 0', '-1 >>> 0', '-1 >>> 1', '1 << 31',
  '1 << 32', '1 << 33', '8 >> 2', '-8 >> 1', '5.9 & 3.1', '~~5.7',
  '1 / 0', '-1 / 0', '0 / 0', '5 % 0', '1 % 0', '3.5 % 1.5',
  '0.1 + 0.2', '0.3 - 0.1', '1 / 3', '2 / 3', '10 / 4',
  'NaN == NaN', 'NaN === NaN', 'NaN != NaN', '1 == "1"', '1 === "1"',
  '0 == false', 'null == undefined', 'null === undefined', '" " == 0',
  '"" == 0', '"0x10" == 16', '".5" == 0.5', '"1e2" == 100', '"abc" == 0',
  '[] == 0', '[1] == 1', '[1,2] == "1,2"', '[null] == ""', '{} == "[object Object]"',
  'undefined == 0', 'null == 0', 'true == 1', '2 == true',
  '-0 * 1', '0 * -1', '0 / -1', '(-0) | 0',
];

// ---- display-safety filter ------------------------------------------------
// |v| >= 1e18 or tiny non-zero: V8 and Rust print these differently.
// -0: V8 prints "-0", alloy prints "0". NaN and ±Infinity are kept (both
// print "NaN", and Infinity is normalized by the runner's sed).
function fragile(v) {
  if (typeof v !== 'number') return false;
  if (Object.is(v, -0)) return true;
  if (Number.isNaN(v)) return false;
  if (!Number.isFinite(v)) return false;
  if (Math.abs(v) >= 1e18) return true;
  if (v !== 0 && Math.abs(v) < 1e-6) return true;
  return false;
}

function safeExpr(e) {
  try {
    // eslint-disable-next-line no-new-func
    const v = Function('a', 'b', 'return (' + e + ');')(3, 7);
    return !fragile(v);
  } catch {
    return false; // a generator bug produced invalid code — drop it loudly below
  }
}

// ---- emit ---------------------------------------------------------------
const exprs = [];
for (const t of TORTURE) exprs.push(t);
while (exprs.length < count) {
  const e = gen(4).s;
  if (safeExpr(e)) exprs.push(e);
}

let js = 'let a = 3, b = 7;\n';
let txt = '';
let dropped = 0;
for (const e of exprs) {
  if (!safeExpr(e)) { dropped++; continue; }
  js += 'print(' + e + ');\n';
  txt += e + '\n';
}
if (dropped > 0) process.stderr.write(`gen: dropped ${dropped} display-fragile/invalid cases\n`);
if (exprs.length - dropped < 10) { process.stderr.write('gen: too few safe cases\n'); process.exit(1); }

require('fs').writeFileSync(prefix + '.js', js);
require('fs').writeFileSync(prefix + '.txt', txt);
process.stderr.write(`gen: seed=${seed} wrote ${exprs.length - dropped} expressions\n`);
