import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";

const EMPTY = { cardNumber: "", expiration: "", securityCode: "", billingName: "", street: "", street2: "" };
const FIELDS = [
  ["cardNumber", "信用卡号码"], ["expiration", "有效期（MM/YY）"],
  ["securityCode", "安全码"], ["billingName", "账单姓名"],
  ["street", "账单地址"], ["street2", "账单地址第二行（可选）"],
] as const;

export function HkCheckoutPayment({ onReady }: { onReady: (ready: boolean) => void }) {
  const [values, setValues] = useState(EMPTY);
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState("");
  useEffect(() => {
    let active = true;
    void invoke<boolean>("checkout_hk_payment_status").then((saved) => {
      if (active) { setReady(saved); onReady(saved); }
    }).catch(() => { if (active) setMessage("无法读取本次运行的付款资料状态"); });
    return () => { active = false; };
  }, [onReady]);
  const update = async (clear: boolean) => {
    setBusy(true);
    try {
      await invoke("set_checkout_hk_payment", { payment: clear ? null : values });
      setValues(EMPTY);
      setReady(!clear);
      onReady(!clear);
      setMessage(clear ? "已清除付款资料" : "已用于本次运行，重启应用后需重新填写");
    } catch {
      setMessage("未能更新：请检查卡号、MM/YY 有效期、安全码和账单资料，并确认当前没有结账正在进行");
    } finally { setBusy(false); }
  };
  return <div className="grid gap-2 rounded-xl border border-border/60 p-3">
    <p className="text-sm">香港信用卡付款 · {ready ? "本次运行已设置" : "未设置"}</p>
    <p className="text-[11px] leading-4 text-muted-foreground">资料仅保留在本次运行的内存中，不写入软件配置。提交订单可能触发信用卡授权或扣款，银行验证仍需手动处理。账单地址须与发卡银行记录一致。</p>
    {FIELDS.map(([key, label]) => <div key={key} className="field-group">
      <Label htmlFor={`hk-${key}`}>{label}</Label>
      <Input id={`hk-${key}`} autoComplete="off" type={key === "cardNumber" || key === "securityCode" ? "password" : "text"}
        value={values[key]} onChange={(event) => setValues({ ...values, [key]: event.target.value })} />
    </div>)}
    <div className="flex flex-wrap gap-2">
      <Button variant="outline" disabled={busy || FIELDS.some(([key]) => key !== "street2" && !values[key].trim())} onClick={() => void update(false)}>用于本次运行</Button>
      {ready ? <Button variant="outline" disabled={busy} onClick={() => void update(true)}>清除付款资料</Button> : null}
    </div>
    {message ? <p className="text-xs" role="status">{message}</p> : null}
  </div>;
}
