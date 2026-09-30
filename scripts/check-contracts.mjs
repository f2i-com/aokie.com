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
 * formlogic harness only compares files that exist in both of ITS trees. The
 * folder is the canonical contract for both repositories (this one owns the
 * plugin protocol) and the rules live in scripts/lib/transfer-contract.mjs:
 *
 *   1. every fixture (*.json) in the folder is listed in SHA256SUMS with a
 *      current digest (SHA-256 of the bytes with CRLF folded to LF, as the
 *      harness does, so a core.autocrlf checkout and the committed blob agree),
 *      and parses;
 *   2. when OAIY_TRANSFER_CONTRACTS names the OAIY repository's copy of the
 *      folder, every fixture AND the document transfer-v1.md must exist there
 *      and be byte-identical (CRLF-normalised); drift is reported per file, as a
 *      fixture or as the document. Unset, the comparison does not run and says
 *      so: the OAIY checkout is not required to build or test this repository.
 *
 * Usage:  node scripts/check-contracts.mjs
 *         node scripts/check-contracts.mjs --write-transfer-sums   (rewrite SHA256SUMS)
 *         node --test scripts/check-contracts.test.mjs             (the transfer rules' own tests)
 * Env:    FORMLOGIC_REPO — path to the formlogic checkout
 *         (default ../formlogic.com, the f2i-com/formlogic.com checkout; then the
 *         legacy C:/wamp64/www/formlogic-app or ../formlogic-app)
 *         OAIY_TRANSFER_CONTRACTS — path to OAIY's docs/contracts/transfer folder
 */

import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { checkTransfer, SUMS, writeSums } from './lib/transfer-contract.mjs';

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

// --- transfer_v1 -----------------------------------------------------------

const transferDir = path.join(repoRoot, 'docs', 'contracts', 'transfer');

if (process.argv.includes('--write-transfer-sums')) {
  console.log(`check-contracts: wrote ${SUMS} (${writeSums(transferDir)} files)`);
  process.exit(0);
}

const transfer = checkTransfer({ transferDir, oaiyDir: process.env.OAIY_TRANSFER_CONTRACTS });
if (transfer.problems.length > 0) {
  console.error(`check-contracts: FAIL — ${transfer.problems.length} transfer_v1 problem(s):`);
  for (const p of transfer.problems) console.error(`  ${p}`);
} else if (transfer.compared === null) {
  console.log(`check-contracts: OK — transfer_v1: ${transfer.files} files match ${SUMS}`);
  console.log('check-contracts: NOTE — the OAIY copy of transfer_v1 was not compared; set OAIY_TRANSFER_CONTRACTS to its docs/contracts/transfer folder.');
} else {
  console.log(`check-contracts: OK — transfer_v1: ${transfer.files} files match ${SUMS}, ${transfer.compared} files (fixtures and the document) byte-identical with the OAIY copy`);
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
