#!/usr/bin/env node
// Checks the manifest at the repository root against the schema published by
// gokite-ai/kite-x402-services, plus the cross-field rules a JSON Schema cannot
// express. Exit 1 on any problem.
//
// Adapted from the official `scripts/validate.mjs` in that repository, which
// walks `services/<name>/service.yaml`. This repository ships exactly one
// manifest, at the root, and the point of running the same schema over it is
// that a service configured here can be submitted to the official catalog
// without being rewritten.
//
//   npm install && npm run check:manifest
//
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import Ajv2020 from "ajv/dist/2020.js";
import addFormats from "ajv-formats";
import { parse } from "yaml";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const manifestPath = join(root, "service.yaml");
const schemaPath = join(root, "schema/service.schema.json");

const schema = JSON.parse(readFileSync(schemaPath, "utf8"));
const ajv = new Ajv2020({ allErrors: true, strictRequired: false });
addFormats(ajv);
const validate = ajv.compile(schema);

const problems = [];

let doc;
try {
  doc = parse(readFileSync(manifestPath, "utf8"));
} catch (err) {
  console.error(`✗ service.yaml: YAML parse error: ${err.message}`);
  process.exit(1);
}

if (!validate(doc)) {
  for (const e of validate.errors) {
    let msg = `service.yaml: ${e.instancePath || "/"} ${e.message}`;
    // The one mistake this schema is most likely to catch, and the least
    // obvious one: an unquoted 0x… address is a YAML integer, and the letters
    // make it either a parse failure or a silently truncated number.
    if (e.instancePath === "/pay_to" && typeof doc?.pay_to === "number") {
      msg += " — quote the address: bare 0x… is parsed as a YAML integer";
    }
    problems.push(msg);
  }
}

// Rules the schema deliberately leaves to the tooling.
if (doc?.schema === 1) {
  const paths = new Set();
  for (const endpoint of doc.endpoints ?? []) {
    const key = `${endpoint.method} ${endpoint.path}`;
    if (paths.has(key)) problems.push(`service.yaml: duplicate endpoint ${key}`);
    paths.add(key);
    // A draft may omit the example; anything that claims to be live may not,
    // because a buyer who guesses wrong pays for the guess.
    if (doc.status !== "draft" && !endpoint.example_request) {
      problems.push(`service.yaml: ${key} needs example_request once it is not a draft`);
    }
  }

  if (!doc.endpoints?.some((e) => e.path.startsWith("/v1/"))) {
    problems.push("service.yaml: every endpoint path must live under /v1/");
  }
}

if (problems.length) {
  console.error(problems.map((p) => `✗ ${p}`).join("\n"));
  process.exit(1);
}

console.log(`✓ service.yaml matches the published Kite schema (${schema.title ?? "service"})`);
