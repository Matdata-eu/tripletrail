// Prints the license text of every package in node_modules (the ones bundled), once each.
const fs = require("fs");
const path = require("path");
const lock = require("./package-lock.json");
const out = [];
for (const [where, pkg] of Object.entries(lock.packages).sort()) {
  if (!where || pkg.dev) continue;
  const dir = path.join(__dirname, where);
  const file = fs.readdirSync(dir).find((f) => /^licen[cs]e/i.test(f));
  const text = file ? fs.readFileSync(path.join(dir, file), "utf8").trim() : "(no license file; license: " + pkg.license + ")";
  out.push(`${where.replace(/^node_modules\//, "")} ${pkg.version} (${pkg.license})\n\n${text}\n`);
}
console.log("CodeMirror 6 and its dependencies, bundled in codemirror.min.js.gz.\n\n" + out.join("\n" + "-".repeat(78) + "\n\n"));
