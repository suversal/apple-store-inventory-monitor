import { useEffect, useState } from "react";
import { AppWindow, LoaderCircle, PlayCircle, ShoppingCart } from "lucide-react";
import { CollapsiblePanel } from "@/components/CollapsiblePanel";
import { BrowserRefreshSettings } from "@/components/BrowserRefreshSettings";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { openCheckoutBrowser, saveCheckoutSettings, testCheckout } from "@/lib/store";
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

/** 监控列表里可以自动结账的不同商品（目前只支持中国大陆）。 */
function monitoredProducts(targets: Target[]): { key: string; name: string }[] {
  const products = new Map<string, string>();
  for (const target of targets) {
    if (target.locale !== "zh_CN") continue;
    const key = productKey(target);
    if (!products.has(key)) {
      products.set(key, target.companionName ? `${target.productName} · ${target.companionName}` : target.productName);
    }
  }
  return [...products].map(([key, name]) => ({ key, name }));
}

export function CheckoutSettings({ checkout, targets }: { checkout: Checkout; targets: Target[] }) {
  const products = monitoredProducts(targets);
  // 只监控一件时就是它；监控多件时以设置里选的为准，选的那件已不在列表里则视为没选。
  const chosen = products.length === 1
    ? products[0]!.key
    : products.some((product) => product.key === checkout.product) ? checkout.product : "";
  const [suffix, setSuffix] = useState(checkout.nationalIdSuffix);
  const [busy, setBusy] = useState<"open" | "test" | null>(null);

  useEffect(() => setSuffix(checkout.nationalIdSuffix), [checkout.nationalIdSuffix]);
  // 设置里存的是旧版手填的其他名称时，也让它出现在下拉框里，不至于显示成空白。
  const bankOptions = INSTALLMENT_BANKS.includes(checkout.installmentBank) || !checkout.installmentBank
    ? INSTALLMENT_BANKS
    : [...INSTALLMENT_BANKS, checkout.installmentBank];

  const save = (patch: Partial<Checkout>) => void saveCheckoutSettings(patch);
  const contact = (key: ContactKey) => (value: string) => save({ [key]: value });
  const suffixReady = checkout.nationalIdSuffix.length === 4;
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
      subtitle="到货后自动走完结账页面，仅支持中国大陆直营店取货"
      summary={summarize(checkout)}
    >
      <div className="grid min-w-0 gap-3">
        <div className="setting-row">
          <Label htmlFor="checkout-enabled" className="text-sm font-normal">到货后自动结账</Label>
          <Switch
            id="checkout-enabled"
            checked={checkout.enabled}
            disabled={(!suffixReady || !chosen) && !checkout.enabled}
            onCheckedChange={(enabled) => save({ enabled })}
          />
        </div>
        <p className="text-[11px] leading-4 break-words text-muted-foreground">
          购物袋里必须只有到货的那一件商品。成功执行一次后会自动关闭，需要时再手动开启。
        </p>
        <div className="field-group">
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
              {products[0]?.name ?? "监控列表里还没有中国大陆的商品"}
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
        </div>

        <div className="field-group">
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
        </div>

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
              <ContactField id="checkout-phone" label="手机号码（不带 +86）" inputMode="numeric" saved={checkout.contactPhone} onSave={contact("contactPhone")} />
            </div>
          </div>
        ) : null}

        <div className="field-group">
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
        </div>

        {checkout.payment === "installment" ? (
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
          {checkout.placeOrder
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
            disabled={busy !== null || !suffixReady || !chosen}
            onClick={() => void run("test", testCheckout)}
          >
            {busy === "test" ? <LoaderCircle className="animate-spin" aria-hidden="true" /> : <PlayCircle aria-hidden="true" />}
            试跑到查看订单页
          </Button>
        </div>
        <p className="text-[11px] leading-4 break-words text-muted-foreground">
          首次使用先打开下单浏览器，登录 Apple 账户、加入商品并手动走一次结账。试跑用「要下单的商品」和它第一条监控项的门店，不会提交订单。
        </p>
        <BrowserRefreshSettings settings={checkout.browserRefresh} />
      </div>
    </CollapsiblePanel>
  );
}
