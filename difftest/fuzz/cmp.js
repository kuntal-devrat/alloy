#!/usr/bin/env node
// Fuzz differential comparator: classify each alloy-vs-Node output line.
//
//   node difftest/fuzz/cmp.js <cases.txt> <alloy.out> <node.out>
//
// Prints one line per differing line and a summary; exits 1 when any line is
// a real mismatch. Classification:
//
//   match      — raw strings identical (NaN, ±Infinity after normalization,
//                strings, undefined, exact numbers)
//   tolerated  — the expression contains `**` AND the two values are finite
//                numbers within 2 ulps. The ES spec lets Number::exponentiate
//                be "implementation-approximated", and V8's pow (llvm-libc)
//                legitimately differs from a correctly-rounded platform powf
//                by 1 ulp on rare near-ties (e.g. `9 ** 17`: 16677181699666570
//                vs 16677181699666568). Not a bug — reported for visibility.
//   mismatch   — anything else: a real semantic divergence.

const fs = require('fs');

const [exprsFile, alloyFile, nodeFile] = process.argv.slice(2);
const exprs = fs.readFileSync(exprsFile, 'utf8').split('\n');
const alloyLines = fs.readFileSync(alloyFile, 'utf8').split('\n');
const nodeLines = fs.readFileSync(nodeFile, 'utf8').split('\n');

function ulpDist(a, b) {
  // a and b are finite numbers here.
  const dv = new DataView(new ArrayBuffer(8));
  dv.setFloat64(0, a);
  const ab = dv.getBigUint64(0);
  dv.setFloat64(0, b);
  const bb = dv.getBigUint64(0);
  return ab > bb ? Number(ab - bb) : Number(bb - ab);
}

let matched = 0;
let tolerated = 0;
let mismatched = 0;

for (let i = 0; i < alloyLines.length; i++) {
  const a = alloyLines[i];
  const n = nodeLines[i];
  if (a === n) {
    matched++;
    continue;
  }
  const expr = (exprs[i] || '').trim();
  const an = Number(a);
  const nn = Number(n);
  // Both must be finite numbers for the ulp tolerance to apply.
  const bothFinite = Number.isFinite(an) && Number.isFinite(nn);
  if (bothFinite && expr.includes('**') && ulpDist(an, nn) <= 2) {
    tolerated++;
    process.stderr.write(`tol ${i + 1}: ${expr} -> alloy ${a}, node ${n}\n`);
  } else {
    mismatched++;
    process.stdout.write(`MISMATCH ${i + 1}: ${expr}\n  alloy: ${a}\n  node:  ${n}\n`);
  }
}

process.stderr.write(
  `cmp: ${matched} matched, ${tolerated} tolerated (pow ulp), ${mismatched} mismatched\n`
);
process.exit(mismatched > 0 ? 1 : 0);
