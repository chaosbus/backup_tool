#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");

const uiDir = __dirname;
const crateDir = path.dirname(uiDir);
const htmlPath = path.join(uiDir, "index.html");
const tauriConfigPath = path.join(crateDir, "tauri.conf.json");
const topLevelInvoke = /^[ \t]*(?:const|let|var)\s+invoke\b/gm;
const scriptTagRe = /<script\b([^>]*)>([\s\S]*?)<\/script>/gi;
const srcAttrRe = /\bsrc\s*=\s*["']([^"']+)["']/i;

function fail(message) {
  console.error(`check-gui: ${message}`);
  process.exit(1);
}

function stripHtmlComments(source) {
  let stripped = "";
  let index = 0;
  while (index < source.length) {
    const start = source.indexOf("<!--", index);
    if (start === -1) {
      stripped += source.slice(index);
      break;
    }
    stripped += source.slice(index, start);
    const end = source.indexOf("-->", start + 4);
    if (end === -1) {
      stripped += " ".repeat(source.length - start);
      break;
    }
    stripped += " ".repeat(end + 3 - start);
    index = end + 3;
  }
  return stripped;
}

function stripJsCommentsAndStrings(source) {
  let stripped = "";
  let i = 0;
  while (i < source.length) {
    const ch = source[i];
    const next = source[i + 1];
    if (ch === "/" && next === "/") {
      while (i < source.length && source[i] !== "\n") {
        stripped += " ";
        i++;
      }
      continue;
    }
    if (ch === "/" && next === "*") {
      stripped += "  ";
      i += 2;
      while (i < source.length && !(source[i] === "*" && source[i + 1] === "/")) {
        stripped += source[i] === "\n" ? "\n" : " ";
        i++;
      }
      stripped += "  ";
      i += 2;
      continue;
    }
    if (ch === '"' || ch === "'" || ch === "`") {
      const quote = ch;
      stripped += " ";
      i++;
      while (i < source.length && source[i] !== quote) {
        if (source[i] === "\\") {
          stripped += "  ";
          i += 2;
        } else {
          stripped += source[i] === "\n" ? "\n" : " ";
          i++;
        }
      }
      if (i < source.length) {
        stripped += " ";
        i++;
      }
      continue;
    }
    stripped += ch;
    i++;
  }
  return stripped;
}

function scriptTags(html) {
  const tags = [];
  const cleanHtml = stripHtmlComments(html);
  let match;
  while ((match = scriptTagRe.exec(cleanHtml)) !== null) {
    tags.push({ attrs: match[1], code: match[2] });
  }
  return tags;
}

function countTopLevelInvoke(source) {
  const clean = stripJsCommentsAndStrings(source);
  return (clean.match(topLevelInvoke) || []).length;
}

function assertNoGlobalTauriUse(source, where) {
  const clean = stripJsCommentsAndStrings(source);
  if (/\bwindow\.__TAURI__\b/.test(clean)) {
    fail(`${where} must not access window.__TAURI__; keep Tauri bindings in loaded app.js`);
  }
}

function normalizeLoadedPath(src) {
  return src.split("?")[0].split("#")[0];
}

const html = fs.readFileSync(htmlPath, "utf8");
const tags = scriptTags(html);
const loaded = [];
const seen = new Set();

for (const tag of tags) {
  const srcMatch = srcAttrRe.exec(tag.attrs);
  if (srcMatch) {
    const raw = srcMatch[1].trim();
    const file = normalizeLoadedPath(raw);
    const resolved = path.resolve(uiDir, file);
    if (seen.has(resolved)) {
      fail(`index.html loads script "${raw}" more than once`);
    }
    seen.add(resolved);
    loaded.push({ raw, file, resolved });
  } else {
    assertNoGlobalTauriUse(tag.code, "inline script");
    if (countTopLevelInvoke(tag.code) > 0) {
      fail("inline script must not declare a top-level invoke");
    }
  }
}

if (!loaded.some((entry) => entry.file === "app.js")) {
  fail('index.html must load ui/app.js via <script src="app.js">');
}

for (const entry of loaded) {
  const relative = path.relative(uiDir, entry.resolved);
  if (relative.startsWith("..") || path.isAbsolute(relative) || path.extname(entry.file) !== ".js") {
    fail(`script src "${entry.raw}" must resolve to a .js file inside ui/`);
  }
  if (!fs.existsSync(entry.resolved)) {
    fail(`script src "${entry.raw}" does not exist at ${entry.resolved}`);
  }
  const count = countTopLevelInvoke(fs.readFileSync(entry.resolved, "utf8"));
  if (path.basename(entry.file) === "app.js") {
    if (entry.resolved !== path.join(uiDir, "app.js")) {
      fail(`only ui/app.js may be named app.js (found ${entry.raw})`);
    }
    if (count !== 1) {
      fail(`app.js must contain exactly one top-level const|let|var invoke declaration (found ${count})`);
    }
  } else if (count !== 0) {
    fail(`${entry.file} must not contain a top-level const|let|var invoke declaration (found ${count})`);
  }
}

const tauriConfig = JSON.parse(fs.readFileSync(tauriConfigPath, "utf8"));
if (tauriConfig.app == null || tauriConfig.app.withGlobalTauri !== true) {
  fail("tauri.conf.json must set app.withGlobalTauri = true");
}

console.log(`check-gui: OK (${loaded.length} external scripts, ${tags.length} script tags)`);
