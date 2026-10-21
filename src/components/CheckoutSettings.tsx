import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { AppWindow, LoaderCircle, PlayCircle, ShoppingCart } from "lucide-react";
import { HkCheckoutPayment } from "@/components/HkCheckoutPayment";
import { CollapsiblePanel } from "@/components/CollapsiblePanel";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { openCheckoutBrowser, saveSettings, testCheckout } from "@/lib/store";
import { INSTALLMENT_PERIODS, type CheckoutPayment, type CheckoutSettings as Checkout, type Target } from "@/lib/types";

const PAYMENT_LABELS: Record<CheckoutPayment, string> = {
  alipay: "支付宝",
  wechat: "微信支付",
  installment: "分期付款",
};

type ContactKey = "contactLastName" | "contactFirstName" | "contactEmail" | "contactPhone";

/** 一项联系信息：输入时只改本地值，离开输入框才保存，避免每敲一个字就写一次设置。 */
function ContactField({
  id, label, saved, onSave, ...input
}: { id: string; label: string; saved: string; onSave: (value: string) => void } & React.ComponentProps<typeof Input>) {
  const [value, setValue] = useState(saved);
  useEffect(() => setValue(saved), [saved]);
  return (
    <div className="field-group">
      <Label htmlFor={id} className="control-label">{label}</Label>
      <Input
        id={id}
        className="control-surface select-text"
        autoComplete="off"
        value={value}
        onChange={(event) => setValue(event.target.value)}
        onBlur={() => {
          if (value.trim() !== saved) onSave(value);
        }}
        {...input}
      />
    </div>
  );
}

/** 结账页「分期付款方案」里各方案的名称，须与页面上的完全一致。 */
const INSTALLMENT_BANKS = ["花呗分期", "微信分付", "招商银行", "中国建设银行", "工商银行"];

/** 花呗分期最多 12 期，其余方案沿用完整的期数列表。 */
function periodsFor(bank: string): readonly number[] {
  return bank === "花呗分期" ? INSTALLMENT_PERIODS.filter((periods) => periods <= 12) : INSTALLMENT_PERIODS;
}

function summarize(checkout: Checkout): string {
  if (!checkout.enabled) return "未开启";
  return checkout.placeOrder ? "到货后自动提交订单" : "到货后停在查看订单页";
}

/** 与后端 checkout_product_key 的规则一致：同一件商品在多家门店的监控项共用一个标识。 */
function productKey(target: Target): string {
  return [target.partNumber, target.companionPart ?? "", target.kitPart ?? ""].join("|");
}

/** 监控列表里可以自动结账的不同商品（按所选结账地区筛选）。 */
function monitoredProducts(targets: Target[], locale: string): { key: string; name: string }[] {
  const products = new Map<string, string>();
  for (const target of targets) {
    if (target.locale !== locale) continue;
    const key = productKey(target);
    if (!products.has(key)) {
      products.set(key, target.companionName ? `${target.productName} · ${target.companionName}` : target.productName);
    }
  }
  return [...products].map(([key, name]) => ({ key, name }));
}

function CheckoutLogin() {
  const [status, setStatus] = useState({ saved: false });
  const [account, setAccount] = useState("");
  const [password, setPassword] = useState("");
  const [message, setMessage] = useState("");
  const [busy, setBusy] = useState(false);
  const refresh = () => invoke<typeof status>("checkout_login_status").then(setStatus);
  useEffect(() => { void refresh().catch(() => setMessage("无法读取自动登录配置")); }, []);
  const save = async () => {
    setBusy(true);
    try {
      await invoke("save_checkout_login", { account, password });
      setPassword("");
      setAccount("");
      await refresh();
      setMessage("已保存到软件配置");
    } catch { setMessage("未能保存，请检查输入或配置文件权限"); }
    finally { setBusy(false); }
  };
  const clear = async () => {
    setBusy(true);
    try { await invoke("clear_checkout_login"); await refresh(); setMessage("已移除自动登录信息"); }
    catch { setMessage("未能移除，请检查配置文件权限"); }
    finally { setBusy(false); }
  };
  return <div className="grid gap-2 rounded-xl border border-border/60 p-3">
    <p className="text-sm">Apple 重新登录{status.saved ? " · 已保存" : " · 未设置"}</p>
    <Label htmlFor="checkout-login-account">Apple 账户</Label>
    <Input id="checkout-login-account" autoComplete="off" value={account} onChange={e => setAccount(e.target.value)} placeholder="电子邮件或电话号码" />
    <Label htmlFor="checkout-login-password">密码</Label>
    <Input id="checkout-login-password" type="password" autoComplete="off" value={password} onChange={e => setPassword(e.target.value)} placeholder={status.saved ? "已保存；输入新密码可更新" : "保存到软件配置"} />
    <div className="flex gap-2">
      <Button variant="outline" disabled={busy || !account.trim() || !password} onClick={() => void save()}>保存登录信息</Button>
      {status.saved ? <Button variant="outline" disabled={busy} onClick={() => void clear()}>移除</Button> : null}
    </div>
    <p className="text-[11px] leading-4 text-muted-foreground">结账要求登录时自动填写并继续。同一次流程只提交一次密码；验证码或二次验证需你处理。账号密码以明文保存在本机 checkout-login.json，仅限当前系统用户访问。请勿分享该配置文件。</p>
    {message ? <p className="text-xs" role="status">{message}</p> : null}
  </div>;
}

export function CheckoutSettings({ checkout, targets }: { checkout: Checkout; targets: Target[] }) {
  const hk = checkout.locale === "zh_HK";
  const [hkPaymentReady, setHkPaymentReady] = useState(false);
  const products = monitoredProducts(targets, checkout.locale);
  // 只监控一件时就是它；监控多件时以设置里选的为准，选的那件已不在列表里则视为没选。
  const chosen = products.length === 1
    ? products[0]!.key
    : products.some((product) => product.key === checkout.product) ? checkout.product : "";
  const targetReady = checkout.autoAddToBag ? products.length > 0 : !!chosen;
  const [suffix, setSuffix] = useState(checkout.nationalIdSuffix);
  const [busy, setBusy] = useState<"open" | "test" | null>(null);

  useEffect(() => setSuffix(checkout.nationalIdSuffix), [checkout.nationalIdSuffix]);
  // 设置里存的是旧版手填的其他名称时，也让它出现在下拉框里，不至于显示成空白。
  const bankOptions = INSTALLMENT_BANKS.includes(checkout.installmentBank) || !checkout.installmentBank
    ? INSTALLMENT_BANKS
    : [...INSTALLMENT_BANKS, checkout.installmentBank];

  const save = (patch: Partial<Checkout>) => void saveSettings({ checkout: { ...checkout, ...patch } });
  const contact = (key: ContactKey) => (value: string) => save({ [key]: value });
  const suffixReady = hk || checkout.nationalIdSuffix.length === 4;
  const paymentReady = !hk || hkPaymentReady;
  const run = async (kind: "open" | "test", action: () => Promise<void>) => {
    setBusy(kind);
    await action();
    setBusy(null);
  };

  return (
    <CollapsiblePanel
      id="checkout"
      icon={<ShoppingCart className="size-4" />}
      title="自动结账"
      subtitle="到货后自动走完结账页面，支持中国大陆和中国香港直营店取货"
      summary={summarize(checkout)}
    >
      <div className="grid min-w-0 gap-3">
        <div className="field-group">
          <Label htmlFor="checkout-locale" className="control-label">结账地区</Label>
          <Select value={checkout.locale} onValueChange={(locale) => save({ locale, enabled: false, placeOrder: false, product: "", contactPhone: "" })}>
            <SelectTrigger id="checkout-locale" className="control-surface w-full"><SelectValue /></SelectTrigger>
            <SelectContent>
              <SelectItem value="zh_CN">中国大陆</SelectItem>
              <SelectItem value="zh_HK">中国香港</SelectItem>
            </SelectContent>
          </Select>
          <p className="text-[11px] leading-4 text-muted-foreground">仅为所选地区结账。切换地区后需重新开启自动结账和自动提交，并确认联系人及付款资料。</p>
        </div>
        {hk ? <>
          <p className="text-[11px] leading-4 text-muted-foreground">香港默认手动登录：点击「打开下单浏览器」完成登录和验证，会话保存在香港专属浏览器资料目录。登录失效时保留页面，完成登录并停在取货方式页后，后续到货自动继续。</p>
          <div className="setting-row">
            <Label htmlFor="checkout-hk-sign-in" className="text-sm font-normal">结账时自动重新登录（可选）</Label>
            <Switch id="checkout-hk-sign-in" checked={checkout.hkAutoSignIn} onCheckedChange={(hkAutoSignIn) => save({ hkAutoSignIn })} />
          </div>
          {checkout.hkAutoSignIn ? <CheckoutLogin /> : null}
          <HkCheckoutPayment onReady={setHkPaymentReady} />
        </> : <CheckoutLogin />}
        <div className="setting-row">
          <Label htmlFor="checkout-enabled" className="text-sm font-normal">到货后自动结账</Label>
          <Switch
            id="checkout-enabled"
            checked={checkout.enabled}
            disabled={(!suffixReady || !targetReady || !paymentReady) && !checkout.enabled}
            onCheckedChange={(enabled) => save({ enabled })}
          />
        </div>
        <p className="text-[11px] leading-4 break-words text-muted-foreground">
          {checkout.autoAddToBag ? <>
            <strong className="text-foreground">开启前建议点击「打开下单浏览器」，手动清空专属购物袋，减少失败清理的等待。</strong>
            <br />
            按本次到货型号加购 1 件；提交前失败会清空专属购物袋，确认空袋后关闭页面并继续监控。清理未成功时，下次先继续清理再加购，自动结账保持开启。
          </> : "购物袋里必须只有到货的那一件商品。"}
        </p>
        <div className="setting-row">
          <Label htmlFor="checkout-disable-after-success" className="text-sm font-normal">成功后关闭自动结账</Label>
          <Switch
            id="checkout-disable-after-success"
            checked={checkout.disableAfterSuccess}
            onCheckedChange={(disableAfterSuccess) => save({ disableAfterSuccess })}
          />
        </div>
        <p className="text-[11px] leading-4 break-words text-muted-foreground">
          {checkout.disableAfterSuccess
            ? "到达查看订单页或确认订单已提交后，关闭上方总开关。"
            : "成功后保持总开关开启；监控继续运行时，持续有货会再次结账并切换当前页面，开启立即下单时可能生成多笔真实订单。"}
          提交结果不明时仍会关闭自动结账并保留页面，避免重复下单。登录需验证时保留页面和开关，完成登录并停在取货方式页后，后续到货自动继续。
        </p>
        <div className="setting-row">
          <Label htmlFor="checkout-auto-add" className="text-sm font-normal">按到货型号自动加入购物袋</Label>
          <Switch id="checkout-auto-add" checked={!!checkout.autoAddToBag} onCheckedChange={(autoAddToBag) => save({ autoAddToBag })} />
        </div>
        <p className="text-[11px] leading-4 break-words text-muted-foreground">
          {checkout.autoAddToBag
            ? "监控列表中所选地区的型号都可触发，同一时间只处理一件。已有本次型号 1 件时沿用；其他商品或数量不符会结束本轮并清空专属购物袋，下次有货重新加购。请勿在此购物袋保存其他商品，暂不支持需另选表带等组合配置。"
            : "关闭时使用事先准备的购物袋，只对下面选定的商品结账。"}
        </p>
        {!checkout.autoAddToBag ? <div className="field-group">
          <Label htmlFor="checkout-product" className="control-label">要下单的商品</Label>
          {products.length > 1 ? (
            <Select value={chosen} onValueChange={(product) => save({ product })}>
              <SelectTrigger id="checkout-product" className="control-surface w-full" aria-label="要下单的商品">
                <SelectValue placeholder="请选择" />
              </SelectTrigger>
              <SelectContent>
                {products.map((product) => (
                  <SelectItem key={product.key} value={product.key}>{product.name}</SelectItem>
                ))}
              </SelectContent>
            </Select>
          ) : (
            <p id="checkout-product" className="text-sm break-words">
              {products[0]?.name ?? "监控列表里还没有所选地区的商品"}
            </p>
          )}
          {products.length > 1 && !chosen ? (
            <p role="alert" className="text-[11px] leading-4 break-words text-destructive">
              监控列表里有 {products.length} 件商品，先选出购物袋里放的那一件，才能开启自动结账。
            </p>
          ) : (
            <p className="text-[11px] leading-4 break-words text-muted-foreground">
              只有这件商品到货才会结账，购物袋里也必须只放它。其他商品到货照常提醒，不会下单。
            </p>
          )}
        </div> : null}

        {!hk ? <div className="field-group">
          <Label htmlFor="checkout-id-suffix" className="control-label">取货人证件号码后四位</Label>
          <Input
            id="checkout-id-suffix"
            className="control-surface select-text tabular-nums"
            placeholder="例如 123X"
            maxLength={4}
            autoComplete="off"
            value={suffix}
            onChange={(event) => setSuffix(event.target.value.replace(/[^0-9a-zA-Z]/g, "").toUpperCase())}
            onBlur={() => {
              if (suffix !== checkout.nationalIdSuffix) save({ nationalIdSuffix: suffix });
            }}
          />
          <p className="text-[11px] leading-4 break-words text-muted-foreground">
            Apple 每次结账都要求填写，且必须与取货时出示的证件一致。只保存在本机的设置文件里。
          </p>
        </div> : null}

        <div className="setting-row">
          <Label htmlFor="checkout-fill-contact" className="text-sm font-normal">按下面的信息填写取货联系人</Label>
          <Switch
            id="checkout-fill-contact"
            checked={checkout.fillContact}
            onCheckedChange={(fillContact) => save({ fillContact })}
          />
        </div>
        <p className="text-[11px] leading-4 break-words text-muted-foreground">
          {checkout.fillContact
            ? "会覆盖结账页上 Apple 预填的内容，留空的项沿用页面上已有的值。姓名须与取货证件一致，下单后无法更改。"
            : "关闭时沿用 Apple 预填的联系信息，只检查有没有缺项。"}
        </p>
        {checkout.fillContact ? (
          <div className="grid min-w-0 grid-cols-2 gap-2">
            <ContactField id="checkout-last-name" label="姓氏" saved={checkout.contactLastName} onSave={contact("contactLastName")} />
            <ContactField id="checkout-first-name" label="名字" saved={checkout.contactFirstName} onSave={contact("contactFirstName")} />
            <div className="col-span-2">
              <ContactField id="checkout-email" label="电子邮件地址" type="email" saved={checkout.contactEmail} onSave={contact("contactEmail")} />
            </div>
            <div className="col-span-2">
              <ContactField id="checkout-phone" label={hk ? "香港手机号码（不带 +852）" : "手机号码（不带 +86）"} inputMode="numeric" saved={checkout.contactPhone} onSave={contact("contactPhone")} />
            </div>
          </div>
        ) : null}

        {!hk ? <div className="field-group">
          <Label htmlFor="checkout-payment" className="control-label">付款方式</Label>
          <Select value={checkout.payment} onValueChange={(payment) => save({ payment: payment as CheckoutPayment })}>
            <SelectTrigger id="checkout-payment" className="control-surface w-full" aria-label="付款方式">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {(Object.keys(PAYMENT_LABELS) as CheckoutPayment[]).map((payment) => (
                <SelectItem key={payment} value={payment}>{PAYMENT_LABELS[payment]}</SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div> : null}

        {!hk && checkout.payment === "installment" ? (
          <div className="grid min-w-0 grid-cols-[minmax(0,1fr)_6rem] gap-2">
            <div className="field-group">
              <Label htmlFor="checkout-bank" className="control-label">分期方案</Label>
              <Select value={checkout.installmentBank} onValueChange={(installmentBank) => {
                const allowed = periodsFor(installmentBank);
                // 换到期数更少的方案时，把超出的期数收到它的最大值。
                const installmentPeriods = allowed.includes(checkout.installmentPeriods)
                  ? checkout.installmentPeriods
                  : Math.max(...allowed);
                save({ installmentBank, installmentPeriods });
              }}>
                <SelectTrigger id="checkout-bank" className="control-surface w-full" aria-label="分期方案">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {bankOptions.map((name) => (
                    <SelectItem key={name} value={name}>{name}</SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <div className="field-group">
              <Label htmlFor="checkout-periods" className="control-label">期数</Label>
              <Select
                value={String(checkout.installmentPeriods)}
                onValueChange={(periods) => save({ installmentPeriods: Number(periods) })}
              >
                <SelectTrigger id="checkout-periods" className="control-surface w-full" aria-label="期数">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {periodsFor(checkout.installmentBank).map((periods) => (
                    <SelectItem key={periods} value={String(periods)}>{periods} 期</SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <p className="col-span-2 text-[11px] leading-4 break-words text-muted-foreground">
              各方案能选的期数不同，结账页上没有所选期数时会停下并列出可选的期数。
            </p>
          </div>
        ) : null}

        <div className="setting-row">
          <Label htmlFor="checkout-nearest-store" className="text-sm font-normal">优先用结账页默认选中的门店</Label>
          <Switch
            id="checkout-nearest-store"
            checked={checkout.acceptNearestStore}
            onCheckedChange={(acceptNearestStore) => save({ acceptNearestStore })}
          />
        </div>
        <p className="text-[11px] leading-4 break-words text-muted-foreground">
          {checkout.acceptNearestStore
            ? "结账页默认选中的最近有货门店可取货时直接在那里取货，可能不是监控到货的那一家。省去换门店的十几秒。"
            : "只在监控到货的门店取货；结账页默认选中的门店也在这件商品的监控列表里时，直接用它。"}
        </p>

        <div className="setting-row">
          <Label htmlFor="checkout-place-order" className="text-sm font-normal">核对通过后自动点「立即下单」</Label>
          <Switch
            id="checkout-place-order"
            checked={checkout.placeOrder}
            onCheckedChange={(placeOrder) => save({ placeOrder })}
          />
        </div>
        <p className="text-[11px] leading-4 break-words text-muted-foreground">
          {hk
            ? checkout.placeOrder
              ? "会提交真实香港订单，并可能触发信用卡授权或扣款；银行验证码及验证页面需手动处理。提交结果不明时关闭自动结账，不会重试提交。"
              : "停在「查看订单」页，由你核对并手动提交。提交香港信用卡订单可能触发授权或扣款。"
            : checkout.placeOrder
              ? "会生成真实订单，下单后取货信息无法更改。付款仍需你在 30 分钟内自己完成。"
              : "关闭时停在「查看订单」页，由你自己点「立即下单」并付款。"}
        </p>

        <div className="grid min-w-0 grid-cols-2 gap-2">
          <Button
            type="button"
            variant="outline"
            className="h-10 rounded-xl border-border/70 bg-background/30"
            disabled={busy !== null}
            onClick={() => void run("open", openCheckoutBrowser)}
          >
            {busy === "open" ? <LoaderCircle className="animate-spin" aria-hidden="true" /> : <AppWindow aria-hidden="true" />}
            打开下单浏览器
          </Button>
          <Button
            type="button"
            variant="outline"
            className="h-10 rounded-xl border-border/70 bg-background/30"
            disabled={busy !== null || !suffixReady || !targetReady || !paymentReady}
            onClick={() => void run("test", testCheckout)}
          >
            {busy === "test" ? <LoaderCircle className="animate-spin" aria-hidden="true" /> : <PlayCircle aria-hidden="true" />}
            试跑到查看订单页
          </Button>
        </div>
        <p className="text-[11px] leading-4 break-words text-muted-foreground">
          {checkout.autoAddToBag
            ? "先在下单浏览器中登录并留空购物袋。试跑会加购监控列表中所选地区的第一件商品，停在查看订单页，不会提交订单；试跑失败保留页面供检查。"
            : "首次使用先打开下单浏览器，登录 Apple 账户、加入商品并手动走一次结账。试跑用「要下单的商品」和它第一条监控项的门店，不会提交订单。"}
        </p>
      </div>
    </CollapsiblePanel>
  );
}
