import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import process from 'node:process';

const repoRoot = process.cwd();
const wireframesDir = path.join(repoRoot, 'docs', 'wireframes');
const indexPath = path.join(wireframesDir, 'index.html');
const indexHtml = readFileSync(indexPath, 'utf8');

function fail(message) {
  console.error(`wireframe page generation failed: ${message}`);
  process.exit(1);
}

function attr(tag, name) {
  return new RegExp(`\\b${name}="([^"]+)"`).exec(tag)?.[1] || '';
}

function pageForScreen(id) {
  return id === 'ob1' ? 'index.html' : `${id}.html`;
}

const navLinks = [...indexHtml.matchAll(/<a\b[^>]*\bdata-s="[^"]+"[^>]*>/g)]
  .map(match => {
    const tag = match[0];
    return { id: attr(tag, 'data-s'), href: attr(tag, 'href') };
  });

if (navLinks.length === 0) {
  fail('no deck navigation links found in docs/wireframes/index.html');
}

const seen = new Set();

mkdirSync(wireframesDir, { recursive: true });

for (const { id, href } of navLinks) {
  if (seen.has(id)) fail(`duplicate deck navigation target "${id}"`);
  seen.add(id);

  const expectedHref = pageForScreen(id);
  if (href !== expectedHref) {
    fail(`navigation target "${id}" links to "${href}", expected "${expectedHref}"`);
  }

  if (!indexHtml.includes(`id="${id}"`)) {
    fail(`navigation target "${id}" does not have a matching screen section`);
  }

  if (href === 'index.html') continue;
  writeFileSync(path.join(wireframesDir, href), indexHtml);
}

console.log(`generated ${seen.size - 1} sibling wireframe pages`);
