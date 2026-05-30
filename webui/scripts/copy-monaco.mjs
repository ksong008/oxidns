/*
 * SPDX-FileCopyrightText: 2025 Sven Shi
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

// Stage Monaco's prebuilt `min/vs` assets into public/monaco/vs so the WebUI
// can self-host the editor instead of pulling it from the jsdelivr CDN at
// runtime. OxiDNS serves this console from the device itself, often on a LAN
// with no outbound internet (or with the CDN blocked/unresolvable), where the
// default CDN loader leaves the YAML editor stuck on "Loading…". Run before
// `next dev` / `next build`; Next copies public/* into the static export.

import { createRequire } from "node:module";
import { cp, mkdir, rm, stat } from "node:fs/promises";
import { dirname, join } from "node:path";

const require = createRequire(import.meta.url);

const monacoPkg = require.resolve("monaco-editor/package.json");
const source = join(dirname(monacoPkg), "min", "vs");
const destination = join(process.cwd(), "public", "monaco", "vs");

const sourceStat = await stat(source).catch(() => null);
if (!sourceStat?.isDirectory()) {
  console.error(`[copy-monaco] source not found: ${source}`);
  process.exit(1);
}

await rm(destination, { recursive: true, force: true });
await mkdir(dirname(destination), { recursive: true });
await cp(source, destination, { recursive: true });

console.log(`[copy-monaco] staged Monaco assets -> ${destination}`);
