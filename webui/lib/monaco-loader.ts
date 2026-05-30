/*
 * SPDX-FileCopyrightText: 2025 Sven Shi
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

import { loader } from "@monaco-editor/react";

// Point @monaco-editor/react at the self-hosted Monaco assets instead of its
// default jsdelivr CDN. OxiDNS serves the WebUI from the device itself, often on
// a LAN with no outbound internet — or with the CDN blocked/unresolvable, at
// times by OxiDNS's own DNS rules. A CDN dependency leaves the YAML editor stuck
// on "Loading…" for any client that can't reach jsdelivr (issue #133). The
// assets are staged into public/monaco/vs by scripts/copy-monaco.mjs and served
// same-origin, so the editor loads offline and on every browser.
//
// Module-level side effect: importing this file configures the loader. It is
// imported by lib/oxidns-yaml-monaco.ts, which every editor entry point already
// pulls in, guaranteeing loader.config() runs before the first loader.init().
if (typeof window !== "undefined") {
  loader.config({ paths: { vs: `${window.location.origin}/monaco/vs` } });
}
