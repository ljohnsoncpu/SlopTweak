// The dev scripts start vite on :1420, but with --strictPort a vite left
// running from another checkout keeps the port and would silently serve a
// different UI. This checks that :1420 serves this checkout's index.html
// (every element id in it is present).

import { readFileSync } from "node:fs";
import { join } from "node:path";

export async function servesThisCheckout(appDir, url = "http://127.0.0.1:1420/") {
  const local = readFileSync(join(appDir, "index.html"), "utf8");
  const ids = [...local.matchAll(/\bid="([^"]+)"/g)].map((m) => m[1]);
  const r = await fetch(url);
  if (!r.ok) return false;
  const served = await r.text();
  const missing = ids.filter((id) => !served.includes(`id="${id}"`));
  if (missing.length) {
    throw new Error(`:1420 serves another checkout (missing ids: ${missing.slice(0, 3).join(", ")}); stop that vite first`);
  }
  return true;
}
