import test from "node:test";
import assert from "node:assert/strict";
import { describeCycleRow, describeCycleSummary, describeMonitorStatus } from "../src/lib/monitorLog.ts";
const target = { storeTitle: "上海-南京东路", storeNumber: "R359", productName: "iPhone", partNumber: "MJTJ4CH/A", locale: "zh_CN" };
const row = (availability, pickupDetails) => ({ target, availability, pickupDetails });
const unavailable = {kind:"out_of_stock"};
const details = {pickupDisplay:"ineligible", pickupQuote:"目前暂不提供 Apple Store 零售店取货服务",saleReason:"NOT_FOR_SALE",saleMessage:"暂未发售"};
test("缺失 SKU 与空取货节点分别解释，不误报结构错误或无货", () => {
  const missing = row({kind:"unknown",reason:"product_not_returned",part_number:target.partNumber});
  const empty = row({kind:"unknown",reason:"no_pickup_data",store_number:"R359"});
  assert.match(describeCycleRow(3,missing), /未返回型号.*R359.*MJTJ4CH\/A.*暂无库存结论/);
  assert.doesNotMatch(describeCycleRow(3,missing), /接口.*异常|停售/);
  assert.equal(describeMonitorStatus(empty).label,"暂无取货数据");
});
test("明确未发售与不支持取货分别显示，单凭 NOT_FOR_SALE 不推断预售", () => {
  assert.equal(describeMonitorStatus(row(unavailable,details)).label,"暂未开售");
  assert.equal(describeMonitorStatus(row(unavailable,{...details,saleMessage:null})).label,"暂不可购买");
  assert.equal(describeMonitorStatus(row(unavailable,{...details,saleReason:null})).label,"不支持取货");
  assert.match(describeCycleRow(1,row(unavailable,details)), /ineligible.*NOT_FOR_SALE.*暂未发售/);
});
test("取货有货不会被送货信息覆盖；无元数据仍兼容旧快照", () => {
  assert.equal(describeMonitorStatus(row({kind:"in_stock"},details)).label,"有货");
  assert.equal(describeMonitorStatus(row(unavailable)).label,"无货");
  assert.equal(describeMonitorStatus(row({kind:"unknown",reason:"transport",detail:"超时"},details)).label,"查询失败");
});
test("每轮汇总按业务原因计数，拦截不能算无货", () => {
  const rows = [row(unavailable,details),row({kind:"unknown",reason:"product_not_returned",part_number:"old"}),row({kind:"unknown",reason:"blocked",detail:"HTTP 541"})];
  assert.equal(describeCycleSummary(rows),"暂未开售 1 项、未返回型号 1 项、请求被拦截 1 项");
  assert.match(describeCycleRow(1,rows[2]), /HTTP 541/);
});

test("Duo 即将发售与18 Pro暂未开售分别显示，日志保留原始区别", () => {
  const duo = row(unavailable,{...details,saleReason:"COMING_SOON",saleMessage:"暂无供应"});
  const status = describeMonitorStatus(duo);
  assert.equal(status.label,"即将发售");
  assert.equal(status.tone,"comingSoon");
  const pro = describeMonitorStatus(row(unavailable,details));
  assert.equal(pro.label,"暂未开售");
  assert.equal(pro.tone,"presale");
  assert.match(describeCycleRow(1,duo), /COMING_SOON.*暂无供应/);
  assert.equal(describeMonitorStatus(row({kind:"in_stock"},duo.pickupDetails)).label,"有货");
});
