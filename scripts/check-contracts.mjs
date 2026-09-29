#!/usr/bin/env node
/**
 * Cross-repository contract digest — aokie side (audit FL-34).
 *
 * The CANONICAL harness lives in the formlogic repo
 * (scripts/check-contracts.mjs); keeping one implementation means the two
 * repos can never disagree about what "in sync" means. This delegator locates
 * the formlogic checkout, points the harness at THIS repo, and propagates its
 * verdict. Missing sibling checkout is a HARD failure (no silent self-skip).
 *
 * The transfer contract (docs/contracts/transfer/, `transfer_v1`, shared with
 * the OAIY repository rather than FormLogic) is checked here first, because the
 * formlogic harness only compares files that exist in both of ITS trees:
 *
 *   1. every fixture (*.json) in the folder is listed in SHA256SUMS with a
 *      current digest (SHA-256 of the bytes with CRLF folded to LF, as the
 *      harness does, so a core.autocrlf checkout and the committed blob agree),
 *      and parses (transfer-v1.md is prose, and each repository's own);
 *   2. when OAIY_TRANSFER_CONTRACTS names the OAIY repository's copy of the
 *      folder, every file of ours must exist there and be byte-identical
 *      (CRLF-normalised). Unset, the comparison does not run and says so: the
 *      OAIY checkout is not required to build or test this repository.
 *
 * Usage:  node scripts/check-contracts.mjs
 *         node scripts/check-contracts.mjs --write-transfer-sums   (rewrite SHA256SUMS)
 * Env:    FORMLOGIC_REPO — path to the formlogic checkout
 *         (default ../formlogic.com, the f2i-com/formlogic.com checkout; then the
 *         legacy C:/wamp64/www/formlogic-app or ../formlogic-app)
 *         OAIY_TRANSFER_CONTRACTS — path to OAIY's docs/contracts/transfer folder
 */

import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

// --- transfer_v1 -----------------------------------------------------------

const transferDir = path.join(repoRoot, 'docs', 'contracts', 'transfer');
const SUMS = 'SHA256SUMS';
const normalized = (buffer) => Buffer.from(buffer.toString('binary').replaceAll('\r\n', '\n'), 'binary');
const sha256 = (buffer) => createHash('sha256').update(normalized(buffer)).digest('hex');

function transferFiles() {
  // The shared set is the fixtures: JSON, byte-identical in both repositories.
  // The prose contract (transfer-v1.md) is each repository's own document.
  return readdirSync(transferDir).filter((name) => name.endsWith('.json')).sort();
}

function checkTransfer() {
  const problems = [];
  if (!existsSync(transferDir)) {
    return { problems: [`transfer contract folder missing: ${transferDir}`], files: 0, compared: null };
  }
  const files = transferFiles();
  for (const name of files) {
    if (!name.endsWith('.json')) continue;
    try {
      JSON.parse(readFileSync(path.join(transferDir, name), 'utf8'));
    } catch (err) {
      problems.push(`transfer contract: invalid JSON ${name} — ${err.message}`);
    }
  }
  const sumsPath = path.join(transferDir, SUMS);
  const listed = new Map();
  if (existsSync(sumsPath)) {
    for (const line of readFileSync(sumsPath, 'utf8').split(/\r?\n/).filter((l) => l.trim())) {
      const match = /^([0-9a-f]{64})  (.+)$/.exec(line);
      if (!match) problems.push(`transfer contract: malformed ${SUMS} line: ${line}`);
      else listed.set(match[2], match[1]);
    }
  } else {
    problems.push(`transfer contract: ${SUMS} missing (run with --write-transfer-sums)`);
  }
  for (const name of files) {
    const actual = sha256(readFileSync(path.join(transferDir, name)));
    if (!listed.has(name)) problems.push(`transfer contract: ${name} is not listed in ${SUMS}`);
    else if (listed.get(name) !== actual) {
      problems.push(`transfer contract: ${name} changed but ${SUMS} was not updated\n    listed ${listed.get(name)}\n    actual ${actual}`);
    }
  }
  for (const name of listed.keys()) {
    if (!files.includes(name)) problems.push(`transfer contract: ${SUMS} lists ${name}, which is not in the folder`);
  }

  let compared = null;
  const oaiyDir = process.env.OAIY_TRANSFER_CONTRACTS;
  if (oaiyDir) {
    compared = 0;
    if (!existsSync(oaiyDir)) {
      problems.push(`transfer contract: OAIY_TRANSFER_CONTRACTS does not exist: ${oaiyDir}`);
    } else {
      const theirs = new Set(readdirSync(oaiyDir));
      for (const name of files) {
        if (!theirs.has(name)) {
          problems.push(`transfer contract: ${name} is missing from the OAIY copy (${oaiyDir})`);
          continue;
        }
        const ours = sha256(readFileSync(path.join(transferDir, name)));
        const other = sha256(readFileSync(path.join(oaiyDir, name)));
        compared++;
        if (ours !== other) problems.push(`transfer contract drift: ${name}\n    aokie ${ours}\n    oaiy  ${other}`);
      }
    }
  }
  return { problems, files: files.length, compared };
}

if (process.argv.includes('--write-transfer-sums')) {
  const lines = transferFiles().map((name) => `${sha256(readFileSync(path.join(transferDir, name)))}  ${name}`);
  writeFileSync(path.join(transferDir, SUMS), `${lines.join('\n')}\n`);
  console.log(`check-contracts: wrote ${SUMS} (${lines.length} files)`);
  process.exit(0);
}

const transfer = checkTransfer();
if (transfer.problems.length > 0) {
  console.error(`check-contracts: FAIL — ${transfer.problems.length} transfer_v1 problem(s):`);
  for (const p of transfer.problems) console.error(`  ${p}`);
} else if (transfer.compared === null) {
  console.log(`check-contracts: OK — transfer_v1: ${transfer.files} files match ${SUMS}`);
  console.log('check-contracts: NOTE — the OAIY copy of transfer_v1 was not compared; set OAIY_TRANSFER_CONTRACTS to its docs/contracts/transfer folder.');
} else {
  console.log(`check-contracts: OK — transfer_v1: ${transfer.files} files match ${SUMS}, ${transfer.compared} byte-identical with the OAIY copy`);
}

// --- the shared FormLogic contracts -----------------------------------------

const formlogicRoot = process.env.FORMLOGIC_REPO
  || [path.join(repoRoot, '..', 'formlogic.com'), 'C:/wamp64/www/formlogic-app', path.join(repoRoot, '..', 'formlogic-app')]
    .find((p) => existsSync(p));

const harness = formlogicRoot && path.join(formlogicRoot, 'scripts', 'check-contracts.mjs');
if (!harness || !existsSync(harness)) {
  console.error('check-contracts: FAIL — formlogic repo (or its scripts/check-contracts.mjs) not found; '
    + 'set FORMLOGIC_REPO. The cross-repo digest cannot run without both checkouts; '
    + 'this is a hard failure by design (FL-34: no silent self-skip).');
  process.exit(1);
}

const res = spawnSync(process.execPath, [harness], {
  stdio: 'inherit',
  env: { ...process.env, AOKIE_REPO: repoRoot },
});
process.exit(transfer.problems.length > 0 ? 1 : (res.status ?? 1));
