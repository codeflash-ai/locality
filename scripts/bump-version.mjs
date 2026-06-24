#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const version = process.argv[2];
const semverReleasePattern =
  /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?$/;

if (!version || !semverReleasePattern.test(version)) {
  console.error("Usage: node scripts/bump-version.mjs <version>");
  console.error("Example: node scripts/bump-version.mjs 0.1.1");
  console.error("Versions must be x.y.z with an optional prerelease suffix.");
  process.exit(2);
}

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const changedFiles = [];

function pathFromRoot(path) {
  return resolve(repoRoot, path);
}

function displayPath(path) {
  return relative(repoRoot, path);
}

function writeIfChanged(path, contents) {
  const previous = readFileSync(path, "utf8");
  if (previous === contents) {
    return;
  }

  writeFileSync(path, contents);
  changedFiles.push(displayPath(path));
}

function updateJson(path, update) {
  const absolutePath = pathFromRoot(path);
  const data = JSON.parse(readFileSync(absolutePath, "utf8"));
  update(data);
  writeIfChanged(absolutePath, `${JSON.stringify(data, null, 2)}\n`);
}

function updateJsonVersionLine(path) {
  const absolutePath = pathFromRoot(path);
  const contents = readFileSync(absolutePath, "utf8");
  const versionLinePattern = /^(\s*"version"\s*:\s*")[^"]+(")(,?)\s*$/m;
  if (!versionLinePattern.test(contents)) {
    throw new Error(`${path} is missing a JSON version field`);
  }

  const updated = contents.replace(versionLinePattern, `$1${version}$2$3`);
  const data = JSON.parse(updated);
  if (data.version !== version) {
    throw new Error(`${path} version did not update correctly`);
  }

  writeIfChanged(absolutePath, updated);
}

function loadCargoMetadata() {
  const output = execFileSync(
    "cargo",
    ["metadata", "--format-version", "1", "--no-deps"],
    {
      cwd: repoRoot,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "inherit"],
    },
  );
  return JSON.parse(output);
}

function packageSectionBounds(contents, path) {
  const packageHeader = /^\[package\]\s*$/m.exec(contents);
  if (!packageHeader) {
    throw new Error(`${displayPath(path)} is missing a [package] section`);
  }

  const sectionStart = packageHeader.index + packageHeader[0].length;
  const rest = contents.slice(sectionStart);
  const nextSection = /^\[/m.exec(rest);
  const sectionEnd =
    nextSection === null ? contents.length : sectionStart + nextSection.index;

  return [sectionStart, sectionEnd];
}

function updateCargoManifest(path) {
  const contents = readFileSync(path, "utf8");
  const [sectionStart, sectionEnd] = packageSectionBounds(contents, path);
  const before = contents.slice(0, sectionStart);
  const section = contents.slice(sectionStart, sectionEnd);
  const after = contents.slice(sectionEnd);

  if (!/^version\s*=/m.test(section)) {
    throw new Error(`${displayPath(path)} [package] section is missing version`);
  }

  const updatedSection = section.replace(
    /^version\s*=\s*"[^"]+"\s*$/m,
    `version = "${version}"`,
  );

  writeIfChanged(path, `${before}${updatedSection}${after}`);
}

function updateCargoLock(path, workspacePackageNames) {
  const contents = readFileSync(path, "utf8");
  const seen = new Set();
  const updated = contents.replace(
    /(\[\[package\]\]\nname = "([^"]+)"\nversion = ")[^"]+(")/g,
    (match, prefix, packageName, suffix) => {
      if (!workspacePackageNames.has(packageName)) {
        return match;
      }

      seen.add(packageName);
      return `${prefix}${version}${suffix}`;
    },
  );

  const missing = [...workspacePackageNames].filter((name) => !seen.has(name));
  if (missing.length > 0) {
    throw new Error(
      `Cargo.lock is missing workspace packages: ${missing.join(", ")}`,
    );
  }

  writeIfChanged(path, updated);
}

updateJson("apps/desktop/package.json", (data) => {
  data.version = version;
});

updateJson("apps/desktop/package-lock.json", (data) => {
  data.version = version;
  if (!data.packages || !data.packages[""]) {
    throw new Error("apps/desktop/package-lock.json is missing packages[\"\"]");
  }
  data.packages[""].version = version;
});

updateJsonVersionLine("apps/desktop/src-tauri/tauri.conf.json");

const cargoMetadata = loadCargoMetadata();
const workspaceMemberIds = new Set(cargoMetadata.workspace_members);
const workspacePackages = cargoMetadata.packages
  .filter((pkg) => workspaceMemberIds.has(pkg.id))
  .sort((a, b) => a.name.localeCompare(b.name));

if (workspacePackages.length === 0) {
  throw new Error("Cargo metadata did not return any workspace packages");
}

for (const pkg of workspacePackages) {
  updateCargoManifest(pkg.manifest_path);
}

updateCargoLock(
  pathFromRoot("Cargo.lock"),
  new Set(workspacePackages.map((pkg) => pkg.name)),
);

if (changedFiles.length === 0) {
  console.log(`Locality release version is already ${version}.`);
} else {
  console.log(`Bumped Locality release version to ${version}.`);
  for (const path of changedFiles) {
    console.log(`  ${path}`);
  }
}
