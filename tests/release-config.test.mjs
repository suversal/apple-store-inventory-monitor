import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

const root = new URL("../", import.meta.url);

test("Linux 包名使用合法 ASCII 标识且保留中文桌面名称", async () => {
  const config = JSON.parse(await readFile(new URL("src-tauri/tauri.linux.conf.json", root), "utf8"));
  assert.match(config.productName, /^[a-z0-9][a-z0-9+.-]*$/);
  assert.equal(config.productName, "guodao-radar");

  const desktop = await readFile(new URL("src-tauri/linux/guodao-radar.desktop.hbs", root), "utf8");
  assert.match(desktop, /^Name=果到雷达$/m);
});

test("公开资产名和 Linux 安装说明使用稳定的 ASCII 文件名", async () => {
  const workflow = await readFile(new URL(".github/workflows/release.yml", root), "utf8");
  const readme = await readFile(new URL("README.md", root), "utf8");
  assert.match(workflow, /releaseAssetNamePattern: "\[mainBinaryName\]_\[version\]_\[arch\]\[setup\]\[ext\]"/);
  assert.match(workflow, /dpkg-query -W.*guodao-radar/);
  assert.match(readme, /apple-store-inventory-monitor_1\.0\.9_amd64\.deb/);
  assert.doesNotMatch(readme, /Apple\.Store\.Inventory\.Monitor_1\.0\.9/);
});
