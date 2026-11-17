import test from "node:test";
import assert from "node:assert/strict";
import {
  mkdtemp,
  mkdir,
  copyFile,
  readFile,
  writeFile,
  rm,
} from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createHash } from "node:crypto";
import { runInNewContext } from "node:vm";

test("SDK patch is idempotent and refuses a changed package or modified chunk", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "sdk-patch-install-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const scripts = join(root, "scripts"),
    pkg = join(root, "node_modules/@limrun/device-install");
  await mkdir(scripts, { recursive: true });
  await mkdir(join(pkg, "dist"), { recursive: true });
  const script = join(scripts, "patch-device-install.mjs");
  const chunk = join(pkg, "dist/index-DJrAlfkC.js");
  await copyFile(
    new URL("../scripts/patch-device-install.mjs", import.meta.url),
    script,
  );
  await copyFile(
    new URL(
      "../node_modules/@limrun/device-install/dist/index-DJrAlfkC.js",
      import.meta.url,
    ),
    chunk,
  );
  await writeFile(join(pkg, "package.json"), '{"version":"0.1.0"}');
  const run = () => spawnSync(process.execPath, [script], { encoding: "utf8" });
  const before = await readFile(chunk, "utf8");
  assert.equal(run().status, 0);
  assert.equal(run().status, 0);
  assert.equal(await readFile(chunk, "utf8"), before);
  // Authenticate the installed chunk: reversing the patch yields the pinned original.
  const scriptSource = await readFile(script, "utf8");
  const definitions = scriptSource.slice(
    scriptSource.indexOf("const originalHash"),
    scriptSource.indexOf("\nfunction digest"),
  );
  const layout = runInNewContext(definitions + "\n({replacements,helpers,marker})");
  assert.ok(before.endsWith(layout.helpers));
  let original = before.slice(0, -layout.helpers.length);
  for (const [from, to] of [...layout.replacements].reverse())
    original = original.replace(to, from);
  assert.equal(
    createHash("sha256").update(original).digest("hex"),
    "da410b47f3150df18f0dab904d16d22f20f0fd2141d0846c1431d4d921375a20",
  );
  // A fresh published chunk is patched to exactly the installed layout.
  await writeFile(chunk, original);
  const fresh = run();
  assert.equal(fresh.status, 0, fresh.stderr);
  assert.equal(await readFile(chunk, "utf8"), before);
  // Files patched by an older layout are refused, not migrated, and left untouched.
  const legacy = original + "\n// phone-use: device-install stream lifecycle v6\n";
  await writeFile(chunk, legacy);
  const old = run();
  assert.notEqual(old.status, 0);
  assert.match(old.stderr, /older layout; reinstall it with `npm ci`/);
  assert.equal(await readFile(chunk, "utf8"), legacy);
  // A changed published chunk fails the pinned checksum.
  await writeFile(chunk, original.replace("async function Q(e, t) {", "async function Q(e, t)  {"));
  const tampered = run();
  assert.notEqual(tampered.status, 0);
  assert.match(tampered.stderr, /ESM checksum|must occur once/);
  await writeFile(chunk, before);
  await writeFile(chunk, before + "\n// unexpected modification\n");
  const changed = run();
  assert.notEqual(changed.status, 0);
  assert.match(changed.stderr, /Unexpected device-install patch contents/);
  assert.equal(
    await readFile(chunk, "utf8"),
    before + "\n// unexpected modification\n",
    "failed verification must not write the package",
  );
  await writeFile(join(pkg, "package.json"), '{"version":"0.1.1"}');
  const upgraded = run();
  assert.notEqual(upgraded.status, 0);
  assert.match(upgraded.stderr, /before upgrading the SDK/);
});
