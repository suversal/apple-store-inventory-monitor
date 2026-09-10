import test from "node:test";
import assert from "node:assert/strict";
import { describeUpdateProgress, describeUpdateError, updatePercent } from "../src/lib/updateStatus.ts";

test("下载累计字节显示实际百分比，总大小未知时仍显示下载量", () => {
  const p = { phase: "downloading", downloaded: 1572864, total: 4194304 };
  assert.equal(updatePercent(p), 37);
  assert.match(describeUpdateProgress(p), /37%.*1\.5 \/ 4\.0 MB/);
  assert.equal(updatePercent({ ...p, total: null }), undefined);
  assert.match(describeUpdateProgress({ ...p, total: null }), /已下载 1\.5 MB/);
  assert.equal(updatePercent({ ...p, total: 0 }), undefined);
});

test("下载完成不冒充安装成功，验证和安装阶段分别展示", () => {
  assert.match(describeUpdateProgress({ phase: "verifying", downloaded: 0, total: null }), /正在验证/);
  assert.match(describeUpdateProgress({ phase: "installing", downloaded: 0, total: null }), /正在安装/);
  assert.match(describeUpdateProgress({ phase: "checking", downloaded: 0, total: null }), /确认更新/);
});

test("实际签名错误说明安装已停止并给出迁移路径", () => {
  const error = describeUpdateError("The signature was created with a different key than the one provided");
  assert.match(error, /签名密钥不匹配，已停止安装/);
  assert.match(error, /完整安装包/);
  assert.match(error, /重复点击下载无法解决/);
  assert.match(describeUpdateError("request timed out"), /连接超时/);
});
