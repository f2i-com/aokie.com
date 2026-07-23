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
 * Usage:  node scripts/check-contracts.mjs
 * Env:    FORMLOGIC_REPO — path to the formlogic checkout
 *         (default C:/wamp64/www/formlogic-app or ../formlogic-app)
 */

import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const formlogicRoot = process.env.FORMLOGIC_REPO
  || ['C:/wamp64/www/formlogic-app', path.join(repoRoot, '..', 'formlogic-app')]
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
process.exit(res.status ?? 1);
