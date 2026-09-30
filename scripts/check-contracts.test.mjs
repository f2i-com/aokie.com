// Tests for the transfer_v1 contract rules (scripts/lib/transfer-contract.mjs) and the
// script that runs them. Run with:  node --test scripts/check-contracts.test.mjs
// Everything happens in temporary copies of docs/contracts/transfer/; nothing outside the
// temporary folder is written.

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { cpSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, describe, it } from 'node:test';
import { fileURLToPath } from 'node:url';
import { checkTransfer, DOCUMENT, findOaiyCopy, SUMS, transferFiles } from './lib/transfer-contract.mjs';

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const realFolder = path.join(repoRoot, 'docs', 'contracts', 'transfer');
const scratch = mkdtempSync(path.join(os.tmpdir(), 'aokie-transfer-contract-'));
after(() => rmSync(scratch, { recursive: true, force: true }));

let counter = 0;
/** A fresh copy of the real folder. */
function copyOfTheFolder() {
  const target = path.join(scratch, `copy-${counter++}`);
  cpSync(realFolder, target, { recursive: true });
  return target;
}
const fixtures = transferFiles(realFolder);
const firstFixture = fixtures[0];
const append = (folder, name, text) => writeFileSync(path.join(folder, name), readFileSync(path.join(folder, name), 'utf8') + text);
const toCrlf = (folder, name) => {
  const file = path.join(folder, name);
  writeFileSync(file, readFileSync(file, 'utf8').replaceAll('\r\n', '\n').replaceAll('\n', '\r\n'));
};

describe('the folder on its own', () => {
  it('passes as committed, and says no copy was compared', () => {
    const result = checkTransfer({ transferDir: realFolder });
    assert.deepEqual(result.problems, []);
    assert.equal(result.compared, null);
    assert.equal(result.files, fixtures.length);
    assert.ok(fixtures.length >= 8);
  });

  it('needs the document, a current SHA256SUMS line for every fixture and valid JSON', () => {
    const noDocument = copyOfTheFolder();
    rmSync(path.join(noDocument, DOCUMENT));
    assert.match(checkTransfer({ transferDir: noDocument }).problems.join('\n'), /document transfer-v1\.md is missing/);

    const stale = copyOfTheFolder();
    append(stale, firstFixture, '\n');
    assert.match(checkTransfer({ transferDir: stale }).problems.join('\n'), new RegExp(`${firstFixture} changed but ${SUMS} was not updated`));

    const unlisted = copyOfTheFolder();
    writeFileSync(path.join(unlisted, 'transfer-v1.extra.fixture.json'), '{}\n');
    assert.match(checkTransfer({ transferDir: unlisted }).problems.join('\n'), /transfer-v1\.extra\.fixture\.json is not listed/);

    const broken = copyOfTheFolder();
    writeFileSync(path.join(broken, firstFixture), '{ not json');
    assert.match(checkTransfer({ transferDir: broken }).problems.join('\n'), new RegExp(`invalid JSON ${firstFixture}`));
  });
});

describe('the comparison with the OAIY copy', () => {
  it('passes for a byte-identical copy and counts the document with the fixtures', () => {
    const oaiy = copyOfTheFolder();
    const result = checkTransfer({ transferDir: realFolder, oaiyDir: oaiy });
    assert.deepEqual(result.problems, []);
    assert.equal(result.compared, fixtures.length + 1, 'every fixture and transfer-v1.md');
  });

  it('folds CRLF to LF in both the fixtures and the document', () => {
    const oaiy = copyOfTheFolder();
    toCrlf(oaiy, DOCUMENT);
    for (const name of fixtures) toCrlf(oaiy, name);
    assert.deepEqual(checkTransfer({ transferDir: realFolder, oaiyDir: oaiy }).problems, []);
  });

  it('reports drift in the document, as the document', () => {
    const oaiy = copyOfTheFolder();
    append(oaiy, DOCUMENT, '\nA sentence only the OAIY copy has.\n');
    const { problems, compared } = checkTransfer({ transferDir: realFolder, oaiyDir: oaiy });
    assert.equal(problems.length, 1, problems.join('\n'));
    assert.match(problems[0], /drift in the document transfer-v1\.md/);
    assert.equal(compared, fixtures.length + 1);
  });

  it('reports drift in a fixture, as a fixture, and not in the document', () => {
    const oaiy = copyOfTheFolder();
    append(oaiy, firstFixture, '\n');
    const { problems } = checkTransfer({ transferDir: realFolder, oaiyDir: oaiy });
    assert.equal(problems.length, 1, problems.join('\n'));
    assert.match(problems[0], new RegExp(`drift in the fixture ${firstFixture}`));
  });

  it('reports drift in both, one problem each', () => {
    const oaiy = copyOfTheFolder();
    append(oaiy, DOCUMENT, '\nx\n');
    append(oaiy, firstFixture, '\n');
    const { problems } = checkTransfer({ transferDir: realFolder, oaiyDir: oaiy });
    assert.equal(problems.length, 2, problems.join('\n'));
    assert.ok(problems.some((problem) => /the document transfer-v1\.md/.test(problem)));
    assert.ok(problems.some((problem) => new RegExp(`the fixture ${firstFixture}`).test(problem)));
  });

  it('reports the document, or a fixture, missing from the copy', () => {
    const noDocument = copyOfTheFolder();
    rmSync(path.join(noDocument, DOCUMENT));
    const missingDocument = checkTransfer({ transferDir: realFolder, oaiyDir: noDocument });
    assert.equal(missingDocument.problems.length, 1);
    assert.match(missingDocument.problems[0], /the document transfer-v1\.md is missing from the OAIY copy/);
    assert.equal(missingDocument.compared, fixtures.length);

    const noFixture = copyOfTheFolder();
    rmSync(path.join(noFixture, firstFixture));
    const missingFixture = checkTransfer({ transferDir: realFolder, oaiyDir: noFixture });
    assert.equal(missingFixture.problems.length, 1);
    assert.match(missingFixture.problems[0], new RegExp(`the fixture ${firstFixture} is missing from the OAIY copy`));
  });

  it('is not troubled by what only OAIY has', () => {
    const oaiy = copyOfTheFolder();
    mkdirSync(path.join(oaiy, 'oaiy-only'));
    writeFileSync(path.join(oaiy, 'oaiy-only', 'phrases-oaiy.json'), '{}\n');
    writeFileSync(path.join(oaiy, SUMS), 'a different list\n');
    assert.deepEqual(checkTransfer({ transferDir: realFolder, oaiyDir: oaiy }).problems, []);
    assert.ok(readdirSync(oaiy).includes('oaiy-only'));
  });

  it('fails when the named copy does not exist', () => {
    const { problems } = checkTransfer({ transferDir: realFolder, oaiyDir: path.join(scratch, 'nowhere') });
    assert.match(problems.join('\n'), /OAIY_TRANSFER_CONTRACTS does not exist/);
  });
});

describe('finding the OAIY copy for the release check', () => {
  const root = path.join(scratch, 'repos', 'aokie');
  const sibling = path.join(scratch, 'repos', 'oaiy', 'docs', 'contracts', 'transfer');

  it('takes the folder OAIY_TRANSFER_CONTRACTS names, before any sibling', () => {
    const found = findOaiyCopy({ env: { OAIY_TRANSFER_CONTRACTS: '/somewhere' }, repoRoot: root, exists: () => true });
    assert.deepEqual(found, { dir: '/somewhere', source: 'OAIY_TRANSFER_CONTRACTS', tried: [] });
  });

  it('else takes the transfer folder of a sibling oaiy checkout', () => {
    const found = findOaiyCopy({ env: {}, repoRoot: root, exists: (p) => p === sibling });
    assert.equal(found.dir, sibling);
    assert.match(found.source, /sibling checkout/);
    // An empty variable names nothing.
    assert.equal(findOaiyCopy({ env: { OAIY_TRANSFER_CONTRACTS: '' }, repoRoot: root, exists: (p) => p === sibling }).dir, sibling);
  });

  it('else says there is none, and where it looked', () => {
    const found = findOaiyCopy({ env: {}, repoRoot: root, exists: () => false });
    assert.equal(found.dir, null);
    assert.deepEqual(found.tried, [sibling]);
  });
});

describe('the release check', () => {
  it('runs the contract check with --find-oaiy, so the comparison with OAIY runs when there is a copy', () => {
    const release = readFileSync(path.join(repoRoot, 'scripts', 'check-release.ps1'), 'utf8');
    const calls = release.split(/\r?\n/).filter((line) => /^\s*node scripts\/check-contracts\.mjs\b/.test(line));
    assert.equal(calls.length, 1, calls.join('\n'));
    assert.match(calls[0], /--find-oaiy/);
  });
});

describe('the script', () => {
  const run = (env, ...args) =>
    spawnSync(process.execPath, [path.join(repoRoot, 'scripts', 'check-contracts.mjs'), ...args], {
      cwd: repoRoot,
      env: { ...process.env, ...env },
      encoding: 'utf8',
    });

  it('with --find-oaiy compares with the copy the variable names, and fails on drift as before', () => {
    const oaiy = copyOfTheFolder();
    const same = run({ OAIY_TRANSFER_CONTRACTS: oaiy }, '--find-oaiy');
    assert.equal(same.status, 0, same.stdout + same.stderr);
    assert.match(same.stdout, /byte-identical with the OAIY copy \(OAIY_TRANSFER_CONTRACTS:/);
    append(oaiy, DOCUMENT, '\ndrift\n');
    const drifted = run({ OAIY_TRANSFER_CONTRACTS: oaiy }, '--find-oaiy');
    assert.equal(drifted.status, 1, drifted.stdout + drifted.stderr);
    assert.match(drifted.stderr, /drift in the document transfer-v1\.md/);
  });

  it('with --find-oaiy and no copy anywhere says SKIPPED, in words, and where it looked', (t) => {
    if (existsSync(path.join(repoRoot, '..', 'oaiy', 'docs', 'contracts', 'transfer'))) {
      t.skip('there is an OAIY checkout next to this repository: the comparison runs instead');
      return;
    }
    const result = run({ OAIY_TRANSFER_CONTRACTS: '' }, '--find-oaiy');
    assert.equal(result.status, 0, result.stdout + result.stderr);
    assert.match(result.stdout, /check-contracts: SKIPPED — the OAIY copy of transfer_v1 was NOT compared/);
    assert.match(result.stdout, /no OAIY checkout at .*oaiy/);
  });

  it('with --find-oaiy compares with a sibling oaiy checkout when the variable is not set', () => {
    // A layout of its own in the scratch folder: <repos>/aokie (this script and
    // the contract) next to <repos>/oaiy (its copy, drifted in the document).
    const repos = path.join(scratch, `layout-${counter++}`);
    const aokie = path.join(repos, 'aokie');
    mkdirSync(path.join(aokie, 'scripts'), { recursive: true });
    cpSync(path.join(repoRoot, 'scripts', 'check-contracts.mjs'), path.join(aokie, 'scripts', 'check-contracts.mjs'));
    cpSync(path.join(repoRoot, 'scripts', 'lib'), path.join(aokie, 'scripts', 'lib'), { recursive: true });
    cpSync(realFolder, path.join(aokie, 'docs', 'contracts', 'transfer'), { recursive: true });
    const theirs = path.join(repos, 'oaiy', 'docs', 'contracts', 'transfer');
    cpSync(realFolder, theirs, { recursive: true });
    const script = path.join(aokie, 'scripts', 'check-contracts.mjs');
    const go = (...args) =>
      spawnSync(process.execPath, [script, ...args], {
        cwd: aokie,
        // No FormLogic checkout next to this layout: that part fails on its own, after the transfer
        // comparison has printed what it found, which is what is read here.
        env: { ...process.env, OAIY_TRANSFER_CONTRACTS: '', FORMLOGIC_REPO: path.join(repos, 'none') },
        encoding: 'utf8',
      });
    const identical = go('--find-oaiy');
    assert.match(identical.stdout, /byte-identical with the OAIY copy \(the sibling checkout \.\.\/oaiy:/, identical.stdout + identical.stderr);
    // Without the flag the sibling is not looked at.
    assert.match(go().stdout, /NOTE — the OAIY copy of transfer_v1 was not compared/);
    // And drift in the sibling is a failure.
    append(theirs, DOCUMENT, '\ndrift\n');
    const drifted = go('--find-oaiy');
    assert.match(drifted.stderr, /drift in the document transfer-v1\.md/, drifted.stdout + drifted.stderr);
  });

  it('without --find-oaiy still compares only what the variable names', () => {
    const result = run({ OAIY_TRANSFER_CONTRACTS: '' });
    assert.equal(result.status, 0, result.stdout + result.stderr);
    assert.match(result.stdout, /check-contracts: NOTE — the OAIY copy of transfer_v1 was not compared/);
  });

  it('fails, naming the document, when the OAIY copy of transfer-v1.md has drifted', () => {
    const oaiy = copyOfTheFolder();
    append(oaiy, DOCUMENT, '\ndrift\n');
    const result = run({ OAIY_TRANSFER_CONTRACTS: oaiy });
    assert.equal(result.status, 1, result.stdout + result.stderr);
    assert.match(result.stderr, /transfer contract drift in the document transfer-v1\.md/);
  });

  it('passes and says the document was compared when the copy is identical', () => {
    const oaiy = copyOfTheFolder();
    const result = run({ OAIY_TRANSFER_CONTRACTS: oaiy });
    assert.equal(result.status, 0, result.stdout + result.stderr);
    assert.match(result.stdout, /fixtures and the document\) byte-identical with the OAIY copy/);
  });
});
