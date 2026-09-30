/**
 * The transfer_v1 contract folder (docs/contracts/transfer/): what "in step"
 * means, in one place, so scripts/check-contracts.mjs and its test agree.
 *
 * The folder is the CANONICAL contract for both repositories (Aokie owns the
 * plugin protocol): the document `transfer-v1.md`, the fixtures
 * (`transfer-v1.<name>.fixture.json`) and `SHA256SUMS`. The OAIY repository
 * keeps a byte-for-byte copy, so a change to either file kind must reach it.
 *
 *   - SHA256SUMS lists every fixture with the SHA-256 of its bytes after CRLF is
 *     folded to LF (so a core.autocrlf checkout and the committed blob agree).
 *     The document is not in SHA256SUMS: it is compared, not listed.
 *   - With an OAIY copy named, every fixture AND the document must exist there
 *     and be byte-identical after the same LF folding. Drift is reported per
 *     file, and says which kind of file drifted.
 */

import { createHash } from 'node:crypto';
import { existsSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';

export const SUMS = 'SHA256SUMS';
export const DOCUMENT = 'transfer-v1.md';

const normalized = (buffer) => Buffer.from(buffer.toString('binary').replaceAll('\r\n', '\n'), 'binary');
export const sha256 = (buffer) => createHash('sha256').update(normalized(buffer)).digest('hex');

/** Where the folder lives in an OAIY checkout. */
export const OAIY_FOLDER = path.join('docs', 'contracts', 'transfer');

/**
 * Where OAIY's copy of the folder is, for a run that should compare with it when it can (the
 * release check): the folder OAIY_TRANSFER_CONTRACTS names, else a sibling `oaiy` checkout of
 * this repository. `dir` is null when there is neither, and `tried` says where it looked, so
 * a skipped comparison can say so and why.
 */
export function findOaiyCopy({ env = process.env, repoRoot, exists = existsSync }) {
  if (env.OAIY_TRANSFER_CONTRACTS) {
    return { dir: env.OAIY_TRANSFER_CONTRACTS, source: 'OAIY_TRANSFER_CONTRACTS', tried: [] };
  }
  const sibling = path.join(repoRoot, '..', 'oaiy', OAIY_FOLDER);
  if (exists(sibling)) return { dir: sibling, source: 'the sibling checkout ../oaiy', tried: [sibling] };
  return { dir: null, source: null, tried: [sibling] };
}

/** The fixtures: JSON files, byte-identical in both repositories and listed in SHA256SUMS. */
export function transferFiles(transferDir) {
  return readdirSync(transferDir).filter((name) => name.endsWith('.json')).sort();
}

/**
 * Check the folder, and, when `oaiyDir` is given, compare it with the OAIY copy.
 * Returns { problems, files, compared }: `compared` is null when no copy was
 * named, otherwise how many files (fixtures and the document) were compared.
 */
export function checkTransfer({ transferDir, oaiyDir }) {
  const problems = [];
  if (!existsSync(transferDir)) {
    return { problems: [`transfer contract folder missing: ${transferDir}`], files: 0, compared: null };
  }
  const files = transferFiles(transferDir);
  for (const name of files) {
    try {
      JSON.parse(readFileSync(path.join(transferDir, name), 'utf8'));
    } catch (err) {
      problems.push(`transfer contract: invalid JSON ${name} — ${err.message}`);
    }
  }
  const documentPath = path.join(transferDir, DOCUMENT);
  if (!existsSync(documentPath)) {
    problems.push(`transfer contract: the document ${DOCUMENT} is missing from the folder`);
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
  if (oaiyDir) {
    compared = 0;
    if (!existsSync(oaiyDir)) {
      problems.push(`transfer contract: OAIY_TRANSFER_CONTRACTS does not exist: ${oaiyDir}`);
    } else {
      const theirs = new Set(readdirSync(oaiyDir));
      const shared = [...files.map((name) => ({ name, kind: 'fixture' }))];
      if (existsSync(documentPath)) shared.push({ name: DOCUMENT, kind: 'document' });
      for (const { name, kind } of shared) {
        if (!theirs.has(name)) {
          problems.push(`transfer contract: the ${kind} ${name} is missing from the OAIY copy (${oaiyDir})`);
          continue;
        }
        const ours = sha256(readFileSync(path.join(transferDir, name)));
        const other = sha256(readFileSync(path.join(oaiyDir, name)));
        compared++;
        if (ours !== other) {
          problems.push(`transfer contract drift in the ${kind} ${name}\n    aokie ${ours}\n    oaiy  ${other}`);
        }
      }
    }
  }
  return { problems, files: files.length, compared };
}

/** Rewrite SHA256SUMS from the fixtures in the folder. */
export function writeSums(transferDir) {
  const lines = transferFiles(transferDir).map((name) => `${sha256(readFileSync(path.join(transferDir, name)))}  ${name}`);
  writeFileSync(path.join(transferDir, SUMS), `${lines.join('\n')}\n`);
  return lines.length;
}
