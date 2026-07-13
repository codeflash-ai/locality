import { readdirSync, readFileSync } from 'node:fs';
import path from 'node:path';
import process from 'node:process';

const repoRoot = process.cwd();
const wireframesDir = path.join(repoRoot, 'docs', 'wireframes');
const indexPath = path.join(wireframesDir, 'index.html');
const indexHtml = readFileSync(indexPath, 'utf8');

function fail(message) {
  console.error(`wireframe page verification failed: ${message}`);
  process.exit(1);
}

function pageForScreen(id) {
  return id === 'ob1' ? 'index.html' : `${id}.html`;
}

const navIds = [...indexHtml.matchAll(/<a\b[^>]*\bdata-s="([^"]+)"/g)].map(match => match[1]);
const sectionIds = [...indexHtml.matchAll(/<section\b[^>]*\bclass="[^"]*\bscreen\b[^"]*"[^>]*\bid="([^"]+)"/g)].map(match => match[1]);

if (navIds.length === 0) {
  fail('deck navigation must use anchors with data-s targets');
}

const uniqueNavIds = new Set(navIds);
const uniqueSectionIds = new Set(sectionIds);

for (const id of uniqueNavIds) {
  if (!uniqueSectionIds.has(id)) {
    fail(`navigation target "${id}" does not have a matching screen section`);
  }
}

for (const id of uniqueSectionIds) {
  if (!uniqueNavIds.has(id)) {
    fail(`screen section "${id}" is missing from deck navigation`);
  }
}

if (!indexHtml.includes('const screenPages = Object.freeze({')) {
  fail('index.html must define a screenPages map');
}

if (!indexHtml.includes('function screenUrl(id)')) {
  fail('index.html must build per-screen HTML URLs');
}

if (!indexHtml.includes('function screenFromLocation()')) {
  fail('index.html must choose the initial screen from the current URL');
}

if (indexHtml.includes('Screen links use URL hashes')) {
  fail('wireframe helper copy still says screen links use URL hashes');
}

const forbiddenTeamNotesMarkers = [
  'team-notes',
  'team-note',
  'Team notes',
  'copy-current-notes',
  'copy-all-notes',
  'copyScreenNotes',
  'copyAllNotes',
  'locality-wireframe-notes',
  'buildTeamNotes'
];

for (const marker of forbiddenTeamNotesMarkers) {
  if (indexHtml.includes(marker)) {
    fail(`team notes marker is still present: ${marker}`);
  }
}

for (const id of uniqueNavIds) {
  const page = pageForScreen(id);
  const linkPattern = new RegExp(`<a\\b(?=[^>]*\\bdata-s="${id}")(?=[^>]*\\bhref="${page}")`, 's');
  if (!linkPattern.test(indexHtml)) {
    fail(`navigation target "${id}" must link to "${page}"`);
  }

  const mapPattern = new RegExp(`${JSON.stringify(id)}\\s*:\\s*${JSON.stringify(page)}`);
  if (!mapPattern.test(indexHtml)) {
    fail(`screenPages is missing "${id}: ${page}"`);
  }

  const pagePath = path.join(wireframesDir, page);
  let pageHtml;
  try {
    pageHtml = readFileSync(pagePath, 'utf8');
  } catch {
    fail(`missing generated page ${path.relative(repoRoot, pagePath)}`);
  }

  if (pageHtml !== indexHtml) {
    fail(`${path.relative(repoRoot, pagePath)} does not match docs/wireframes/index.html`);
  }
}

const expectedPages = new Set([...uniqueNavIds].map(pageForScreen));
const actualHtmlPages = readdirSync(wireframesDir)
  .filter(name => name.endsWith('.html'))
  .sort();

for (const page of actualHtmlPages) {
  if (!expectedPages.has(page)) {
    fail(`unexpected HTML page in wireframes directory: ${page}`);
  }
}

console.log(`verified ${expectedPages.size} wireframe HTML pages`);
