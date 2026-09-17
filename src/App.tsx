import { useEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import packageInfo from "../package.json";
import {
  Activity,
  AlertTriangle,
  BellRing,
  Clock3,
  Download,
  MapPin,
  PackageCheck,
  PackageX,
  Pause,
  Play,
  Plus,
  Radar,
  RefreshCw,
  Settings2,
  ShoppingBag,
  Smartphone,
  SquareTerminal,
  Trash2,
  Truck,
  Volume2,
  X,
} from "lucide-react";

import { describeUpdateProgress, updatePercent } from "@/lib/updateStatus";
import { Combobox } from "@/components/Combobox";
import { MultiCombobox } from "@/components/MultiCombobox";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Popover,
  PopoverContent,
  PopoverDescription,
  PopoverHeader,
  PopoverTitle,
  PopoverTrigger,
} from "@/components/ui/popover";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";

import {
  changeLocale,
  connect,
  dismissUpdate,
  installUpdate,
  loadDeliveryLocalities,
  loadWatchBandChoices,
  loadWatchBandSizes,
  openAuthorPage,
  openProjectPage,
  openReleasePage,
  openTargetProduct,
  refreshProducts,
  saveSettings,
  setCategory,
  setIntervalSeconds,
  setProductBarkUrl,
  setTargets,
  startWatching,
  stopWatching,
  testNotify,
  watcherStore,
} from "@/lib/store";
import {
  type Availability,
  type DeliveryLocalities,
  type DeliveryRegion,
  type PickupDetails,
  type Category,
  type OpenOnHit,
  describeAdvice,
  formatTime,
  isUntrusted,
  type StatusTone,
  type Target,
  type WatchBandChoice,
  type WatchBandSize,
  targetKey,
} from "@/lib/types";

import { describeDelivery, describeMonitorStatus, describePickupDate, type DeliveryTone } from "@/lib/monitorLog";
import { compareNewestProducts, sortMonitorsNewestFirst } from "@/lib/productOrder";

const EMPTY_DELIVERY_LOCALITIES: DeliveryLocalities = {
  states: [],
  cities: [],
  districts: [],
};

function compactProductName(name: string): string {
  if (!name.startsWith("Apple Watch ")) return name;
  return name
    .replace(/^Apple Watch\s+/, "")
    .replace(/\s+新外观(?=\s|$)/g, "")
    .replace(/(\d+)\s*毫米/g, "$1mm")
    .replace(/\s+/g, " ")
    .trim();
}

function watchBandDescription(companionPart?: string): string | undefined {
  return companionPart ? `送货查询使用目录默认表带 ${companionPart}` : undefined;
}

function GithubBrandIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <path fillRule="evenodd" d="M12 2C6.477 2 2 6.589 2 12.253c0 4.53 2.865 8.374 6.839 9.731.5.094.682-.222.682-.494 0-.244-.009-.888-.014-1.744-2.782.62-3.369-1.374-3.369-1.374-.455-1.184-1.11-1.499-1.11-1.499-.908-.636.069-.623.069-.623 1.004.073 1.532 1.057 1.532 1.057.892 1.568 2.341 1.115 2.91.853.091-.663.349-1.115.635-1.371-2.221-.259-4.555-1.14-4.555-5.067 0-1.119.389-2.034 1.029-2.751-.103-.26-.446-1.302.098-2.713 0 0 .84-.276 2.75 1.051A9.33 9.33 0 0 1 12 7.992a9.31 9.31 0 0 1 2.504.346c1.909-1.327 2.748-1.051 2.748-1.051.545 1.411.202 2.453.1 2.713.64.717 1.027 1.632 1.027 2.751 0 3.937-2.337 4.805-4.565 5.059.359.317.679.944.679 1.903 0 1.374-.012 2.482-.012 2.819 0 .275.18.593.688.493C19.14 20.625 22 16.783 22 12.253 22 6.589 17.523 2 12 2Z" clipRule="evenodd" />
    </svg>
  );
}

function XBrandIcon() {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <path d="M18.244 2.25h3.308l-7.227 8.26 8.502 11.24H16.17l-5.214-6.817L4.99 21.75H1.68l7.73-8.835L1.254 2.25H8.08l4.713 6.231 5.45-6.231Zm-1.161 17.52h1.833L7.084 4.126H5.117L17.083 19.77Z" />
    </svg>
  );
}

const TONE_CLASS: Record<StatusTone, string> = {
  inStock: "bg-in-stock/12 text-in-stock border-in-stock/25",
  outOfStock: "bg-out-of-stock/12 text-out-of-stock border-out-of-stock/30",
  presale: "bg-presale/12 text-presale border-presale/30",
  comingSoon: "bg-coming-soon/12 text-coming-soon border-coming-soon/30",
  pickupUnsupported: "bg-pickup-unsupported/12 text-pickup-unsupported border-pickup-unsupported/30",
  notForSale: "bg-not-for-sale/12 text-not-for-sale border-not-for-sale/30",
  unknown: "bg-unknown/12 text-unknown border-unknown/30",
  pending: "bg-transparent text-muted-foreground/70 border-border border-dashed",
};

const TONE_DOT: Record<StatusTone, string> = {
  inStock: "bg-in-stock shadow-[0_0_10px_var(--in-stock)]",
  outOfStock: "bg-out-of-stock",
  presale: "bg-presale",
  comingSoon: "bg-coming-soon",
  pickupUnsupported: "bg-pickup-unsupported",
  notForSale: "bg-not-for-sale",
  unknown: "bg-unknown",
  pending: "bg-muted-foreground/50",
};

function StatusBadge({ availability, pickupDetails }: { availability: Availability; pickupDetails?: PickupDetails }) {
  const { label, tone, detail } = describeMonitorStatus({ availability, pickupDetails });
  const pickupDate = availability.kind === "in_stock" ? describePickupDate(pickupDetails?.pickupQuote) : null;
  const badge = (
    <span className="inline-flex flex-col items-center gap-0.5">
      <Badge
        variant="outline"
        className={`h-7 min-w-20 justify-center gap-2 px-2.5 font-medium ${TONE_CLASS[tone]}`}
      >
        <span className={`size-1.5 rounded-full ${TONE_DOT[tone]}`} aria-hidden="true" />
        {label}
      </Badge>
      {pickupDate ? <span className="text-[10px] leading-3 text-in-stock tabular-nums">{pickupDate} 可取</span> : null}
    </span>
  );
  if (!detail) return badge;
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className="cursor-help">{badge}</span>
      </TooltipTrigger>
      <TooltipContent className="max-w-90">{detail}</TooltipContent>
    </Tooltip>
  );
}

const DELIVERY_CLASS: Record<DeliveryTone, string> = {
  fast: "border-delivery-fast/30 bg-delivery-fast/12 text-delivery-fast",
  soon: "border-delivery-soon/30 bg-delivery-soon/12 text-delivery-soon",
  standard: "border-delivery-standard/30 bg-delivery-standard/12 text-delivery-standard",
  later: "border-delivery-later/35 bg-delivery-later/12 text-delivery-later",
  unavailable: "border-border bg-muted/35 text-muted-foreground",
  unknown: "border-delivery-unknown/30 bg-delivery-unknown/10 text-delivery-unknown",
};

function DeliveryBadge({ pickupDetails, lastCheckedMs }: { pickupDetails?: PickupDetails; lastCheckedMs: number | null }) {
  const delivery = describeDelivery(pickupDetails, lastCheckedMs);
  if (!delivery) {
    return <span className="text-xs text-muted-foreground/60" title="Apple 本轮未返回预计送货时间">未返回</span>;
  }

  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span
          className={`inline-grid min-w-24 max-w-48 cursor-pointer grid-cols-[auto_minmax(0,1fr)] items-center gap-x-1.5 rounded-lg border px-2 py-1.5 ${DELIVERY_CLASS[delivery.tone]}`}
          aria-label={`预计送货：${delivery.detail}；${delivery.timing}`}
        >
          <span className="size-1.5 rounded-full bg-current" aria-hidden="true" />
          <span className="whitespace-nowrap text-xs font-semibold tabular-nums">{delivery.label}</span>
          <span className="col-start-2 whitespace-normal text-[10px] leading-3 opacity-80">{delivery.timing}</span>
        </span>
      </TooltipTrigger>
      <TooltipContent className="max-w-90">Apple 预计送货：{delivery.detail}</TooltipContent>
    </Tooltip>
  );
}

function ProductBarkRoute({
  target,
  customUrl,
  disabled,
}: {
  target: Target;
  customUrl?: string;
  disabled: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [draft, setDraft] = useState("");
  const [saving, setSaving] = useState(false);
  const hasCustomUrl = Boolean(customUrl);

  async function save() {
    setSaving(true);
    try {
      if (await setProductBarkUrl(target, draft)) setOpen(false);
    } finally {
      setSaving(false);
    }
  }

  return (
    <Popover
      open={open}
      onOpenChange={(nextOpen) => {
        if (nextOpen) setDraft(customUrl ?? "");
        setOpen(nextOpen);
      }}
    >
      <PopoverTrigger asChild>
        <Button
          variant="ghost"
          size="sm"
          className={hasCustomUrl ? "text-primary" : "text-muted-foreground"}
          aria-label={`${target.productName}：${hasCustomUrl ? "已设置专属 Bark" : "使用默认 Bark"}`}
          disabled={disabled}
        >
          <BellRing aria-hidden="true" />
          {hasCustomUrl ? "专属" : "默认"}
        </Button>
      </PopoverTrigger>
      <PopoverContent align="end" className="w-96 max-w-[calc(100vw-2rem)] space-y-4 rounded-xl">
        <PopoverHeader>
          <PopoverTitle>此型号的 Bark 推送</PopoverTitle>
          <PopoverDescription className="break-words leading-5">
            {target.productName}。同一型号在不同门店共用此地址；留空则沿用右侧监控设置里的默认 Bark。
          </PopoverDescription>
        </PopoverHeader>
        <div className="field-group">
          <Label htmlFor={`product-bark-${targetKey(target)}`} className="control-label">
            专属 Bark 地址
          </Label>
          <Input
            id={`product-bark-${targetKey(target)}`}
            type="url"
            className="control-surface select-text"
            placeholder="https://api.day.app/对方的BarkKey"
            value={draft}
            onChange={(event) => setDraft(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") void save();
            }}
            disabled={saving}
          />
        </div>
        <div className="flex justify-end gap-2">
          <Button variant="ghost" size="sm" onClick={() => setOpen(false)} disabled={saving}>
            取消
          </Button>
          <Button size="sm" onClick={() => void save()} disabled={saving}>
            {saving ? "保存中…" : draft.trim() ? "保存专属地址" : "使用默认地址"}
          </Button>
        </div>
      </PopoverContent>
    </Popover>
  );
}

function watchBandSizeLabel(text: string): string {
  return /^\d+$/.test(text.trim()) ? `${text.trim()} 号` : text.trim();
}

function WatchBandPicker({
  target,
  targets,
  defaultCompanionPart,
  disabled,
}: {
  target: Target;
  targets: Target[];
  defaultCompanionPart?: string;
  disabled: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [choices, setChoices] = useState<WatchBandChoice[]>([]);
  const [sizes, setSizes] = useState<WatchBandSize[]>([]);
  const [choiceValue, setChoiceValue] = useState("");
  const [selectedPart, setSelectedPart] = useState("");
  const [loadingChoices, setLoadingChoices] = useState(false);
  const [loadingSizes, setLoadingSizes] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const currentLabel = target.companionName
    ? target.companionName
    : `目录默认表带 ${target.companionPart ?? "未设置"}`;

  async function openPicker(nextOpen: boolean) {
    setOpen(nextOpen);
    if (!nextOpen || choices.length > 0 || loadingChoices) return;
    setLoadingChoices(true);
    setError(null);
    try {
      setChoices(await loadWatchBandChoices(target.locale, target.partNumber));
    } catch (reason) {
      setError(`读取 Apple 表带选项失败：${String(reason)}`);
    } finally {
      setLoadingChoices(false);
    }
  }

  async function chooseBand(value: string) {
    setChoiceValue(value);
    setSelectedPart("");
    setSizes([]);
    const choice = choices.find((item) => `${item.styleKey}::${item.colorKey}` === value);
    if (!choice) return;
    setLoadingSizes(true);
    setError(null);
    try {
      const nextSizes = await loadWatchBandSizes(
        target.locale,
        target.partNumber,
        choice.styleKey,
        choice.colorKey,
      );
      setSizes(nextSizes);
      const onlySize = nextSizes.length === 1 ? nextSizes.at(0) : undefined;
      if (onlySize) setSelectedPart(onlySize.partNumber);
    } catch (reason) {
      setError(`读取 Apple 表带尺码失败：${String(reason)}`);
    } finally {
      setLoadingSizes(false);
    }
  }

  async function applyBand() {
    const choice = choices.find((item) => `${item.styleKey}::${item.colorKey}` === choiceValue);
    const size = sizes.find((item) => item.partNumber === selectedPart);
    if (!choice || !size) return;
    setSaving(true);
    const companionName = `${choice.styleName} · ${choice.colorName} · ${watchBandSizeLabel(size.text)}`;
    const saved = await setTargets(targets.map((item) => (
      item.locale === target.locale && item.partNumber === target.partNumber
        ? { ...item, companionPart: size.partNumber, companionName }
        : item
    )));
    setSaving(false);
    if (saved) setOpen(false);
  }

  async function restoreAutomaticBand() {
    if (!defaultCompanionPart) return;
    setSaving(true);
    const saved = await setTargets(targets.map((item) => (
      item.locale === target.locale && item.partNumber === target.partNumber
        ? { ...item, companionPart: defaultCompanionPart, companionName: undefined }
        : item
    )));
    setSaving(false);
    if (saved) setOpen(false);
  }

  return (
    <Popover open={open} onOpenChange={(nextOpen) => void openPicker(nextOpen)}>
      <PopoverTrigger asChild>
        <button
          type="button"
          className="mt-0.5 block max-w-full truncate text-left text-[11px] leading-4 text-muted-foreground/75 hover:text-primary hover:underline"
          title={`${currentLabel}；点击按官网款式、颜色和尺码修改`}
          disabled={disabled}
        >
          送货表带：{currentLabel}
        </button>
      </PopoverTrigger>
      <PopoverContent align="start" className="w-[28rem] max-w-[calc(100vw-2rem)] space-y-4 rounded-xl">
        <PopoverHeader>
          <PopoverTitle>选择精确送货表带</PopoverTitle>
          <PopoverDescription className="leading-5">
            同一表壳在所有门店共用这条表带。送货日期取决于表壳、表带和尺码的完整组合。
          </PopoverDescription>
        </PopoverHeader>
        <div className="grid gap-3">
          <div className="field-group">
            <Label className="control-label">款式与颜色</Label>
            <Select value={choiceValue} onValueChange={(value) => void chooseBand(value)} disabled={loadingChoices || saving}>
              <SelectTrigger className="control-surface w-full">
                <SelectValue placeholder={loadingChoices ? "正在读取 Apple 选项…" : "选择表带款式和颜色"} />
              </SelectTrigger>
              <SelectContent>
                {choices.map((choice) => (
                  <SelectItem
                    key={`${choice.styleKey}::${choice.colorKey}`}
                    value={`${choice.styleKey}::${choice.colorKey}`}
                  >
                    {choice.styleName} · {choice.colorName}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          <div className="field-group">
            <Label className="control-label">尺码</Label>
            <Select value={selectedPart} onValueChange={setSelectedPart} disabled={!choiceValue || loadingSizes || saving}>
              <SelectTrigger className="control-surface w-full">
                <SelectValue placeholder={loadingSizes ? "正在读取 Apple 尺码…" : "选择尺码"} />
              </SelectTrigger>
              <SelectContent>
                {sizes.map((size) => (
                  <SelectItem key={size.partNumber} value={size.partNumber}>
                    {watchBandSizeLabel(size.text)} · {size.partNumber}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          {error ? <p role="alert" className="text-xs leading-5 text-destructive">{error}</p> : null}
        </div>
        <div className="flex items-center justify-between gap-2">
          <Button variant="ghost" size="sm" onClick={() => void restoreAutomaticBand()} disabled={!defaultCompanionPart || saving}>
            恢复自动搭配
          </Button>
          <div className="flex gap-2">
            <Button variant="ghost" size="sm" onClick={() => setOpen(false)} disabled={saving}>取消</Button>
            <Button size="sm" onClick={() => void applyBand()} disabled={!selectedPart || saving}>
              {saving ? "保存中…" : "应用到同型号"}
            </Button>
          </div>
        </div>
      </PopoverContent>
    </Popover>
  );
}

export default function App() {
  const ui = useSyncExternalStore(watcherStore.subscribe, watcherStore.getSnapshot);
  const [clockMs, setClockMs] = useState(() => Date.now());

  useEffect(() => {
    void connect();
  }, []);

  useEffect(() => {
    if (!ui.running) return;
    setClockMs(Date.now());
    const timer = window.setInterval(() => setClockMs(Date.now()), 1_000);
    return () => window.clearInterval(timer);
  }, [ui.running]);

  const [storeNumbers, setStoreNumbers] = useState<string[]>([]);
  const [partNumbers, setPartNumbers] = useState<string[]>([]);
  const [isAdding, setIsAdding] = useState(false);
  const [barkDraft, setBarkDraft] = useState<string | null>(null);
  const [intervalDraft, setIntervalDraft] = useState<number | null>(null);
  const [deliveryDraft, setDeliveryDraft] = useState<DeliveryRegion>({ state: "", city: "", district: "" });
  const [deliveryOptions, setDeliveryOptions] = useState<DeliveryLocalities>(EMPTY_DELIVERY_LOCALITIES);
  const [deliveryOptionsLoading, setDeliveryOptionsLoading] = useState(false);
  const [deliveryDirty, setDeliveryDirty] = useState(false);
  const [deliveryError, setDeliveryError] = useState<string | null>(null);
  const [deliverySaving, setDeliverySaving] = useState(false);
  const deliveryRequestId = useRef(0);

  useEffect(() => {
    if (!ui.ready || deliveryDirty) return;
    setDeliveryDraft(ui.settings.deliveryRegion ?? { state: "", city: "", district: "" });
  }, [ui.ready, ui.settings.deliveryRegion, deliveryDirty]);

  useEffect(() => {
    if (!ui.ready) return;
    if (ui.settings.locale !== "zh_CN") {
      setDeliveryOptions(EMPTY_DELIVERY_LOCALITIES);
      setDeliveryOptionsLoading(false);
      setDeliveryError("Apple 官网当前仅在中国大陆站提供省、市、区三级选择。");
      return;
    }

    const selected = ui.settings.deliveryRegion;
    const requestId = ++deliveryRequestId.current;
    setDeliveryOptionsLoading(true);
    void loadDeliveryLocalities(ui.settings.locale, selected?.state, selected?.city)
      .then((options) => {
        if (deliveryRequestId.current !== requestId) return;
        setDeliveryOptions(options);
        if (selected && (
          !options.states.some((option) => option.value === selected.state)
          || !options.cities.some((option) => option.value === selected.city)
          || !options.districts.some((option) => option.value === selected.district)
        )) {
          setDeliveryDraft({ state: "", city: "", district: "" });
          setDeliveryDirty(true);
          setDeliveryError("此前保存的地址不是 Apple 官方选项，请重新选择。");
        } else {
          setDeliveryError(null);
        }
      })
      .catch((error) => {
        if (deliveryRequestId.current !== requestId) return;
        setDeliveryOptions(EMPTY_DELIVERY_LOCALITIES);
        setDeliveryError(`加载 Apple 地区选项失败：${String(error)}`);
      })
      .finally(() => {
        if (deliveryRequestId.current === requestId) setDeliveryOptionsLoading(false);
      });
  }, [ui.ready, ui.settings.deliveryRegion, ui.settings.locale]);

  async function refreshDeliveryOptions(stateName: string, cityName = "") {
    const requestId = ++deliveryRequestId.current;
    setDeliveryOptionsLoading(true);
    setDeliveryError(null);
    try {
      const options = await loadDeliveryLocalities(ui.settings.locale, stateName, cityName);
      if (deliveryRequestId.current === requestId) setDeliveryOptions(options);
    } catch (error) {
      if (deliveryRequestId.current === requestId) {
        setDeliveryError(`加载 Apple 地区选项失败：${String(error)}`);
      }
    } finally {
      if (deliveryRequestId.current === requestId) setDeliveryOptionsLoading(false);
    }
  }

  async function persistDeliveryRegion(region: DeliveryRegion) {
    setDeliverySaving(true);
    setDeliveryError(null);
    const saved = await saveSettings({ deliveryRegion: region });
    if (saved) {
      setDeliveryDirty(false);
    } else {
      setDeliveryError("送货地区保存失败，请重新选择区或稍后重试。");
    }
    setDeliverySaving(false);
  }

  const barkValue = barkDraft ?? ui.settings.barkUrl;
  const intervalValue = intervalDraft ?? ui.settings.intervalSeconds;
  const deliverySelectionValid =
    deliveryOptions.states.some((option) => option.value === deliveryDraft.state)
    && deliveryOptions.cities.some((option) => option.value === deliveryDraft.city)
    && deliveryOptions.districts.some((option) => option.value === deliveryDraft.district);
  const secondsUntilNextCheck = ui.nextCheckAtMs
    ? Math.max(0, Math.ceil((ui.nextCheckAtMs - clockMs) / 1_000))
    : null;
  const runningLabel =
    secondsUntilNextCheck === null || secondsUntilNextCheck === 0
      ? "正在查询"
      : `约 ${secondsUntilNextCheck} 秒后检查${ui.paced ? " · 保护节奏" : ""}`;

  const storeOptions = useMemo(
    () => ui.stores.map((store) => ({ value: store.number, label: store.title })),
    [ui.stores],
  );
  const productOptions = useMemo(
    () =>
      ui.products
        .filter((product) => product.category === ui.category)
        .sort(compareNewestProducts)
        .map((product) => ({
          value: product.partNumber,
          label: compactProductName(product.title),
          description: product.category === "watch"
            ? watchBandDescription(product.companionPart)
            : undefined,
        })),
    [ui.products, ui.category],
  );

  const sortedRows = useMemo(() => sortMonitorsNewestFirst(ui.rows), [ui.rows]);

  const targets = useMemo(() => ui.rows.map((row) => row.target), [ui.rows]);

  useEffect(() => {
    const valid = new Set(storeOptions.map((option) => option.value));
    setStoreNumbers((previous) => {
      const next = previous.filter((value) => valid.has(value));
      return next.length === previous.length ? previous : next;
    });
  }, [storeOptions]);

  useEffect(() => {
    const valid = new Set(productOptions.map((option) => option.value));
    setPartNumbers((previous) => {
      const next = previous.filter((value) => valid.has(value));
      return next.length === previous.length ? previous : next;
    });
  }, [productOptions]);

  const summary = useMemo(() => {
    let inStock = 0;
    let outOfStock = 0;
    let untrusted = 0;
    for (const row of ui.rows) {
      if (row.availability.kind === "in_stock") inStock += 1;
      else if (row.availability.kind === "out_of_stock") outOfStock += 1;
      if (isUntrusted(row.availability)) untrusted += 1;
    }
    return { inStock, outOfStock, untrusted };
  }, [ui.rows]);

  const pendingTargets = useMemo(() => {
    const existing = new Set(targets.map(targetKey));
    const stores = new Map(ui.stores.map((store) => [store.number, store]));
    const products = new Map(ui.products.map((product) => [product.partNumber, product]));
    const pending: Target[] = [];

    for (const storeNumber of storeNumbers) {
      const store = stores.get(storeNumber);
      if (!store) continue;
      for (const partNumber of partNumbers) {
        const product = products.get(partNumber);
        if (!product || product.category !== ui.category) continue;
        const target: Target = {
          locale: ui.settings.locale,
          storeNumber: store.number,
          storeTitle: store.title,
          partNumber: product.partNumber,
          productName: product.title,
          ...(product.companionPart ? { companionPart: product.companionPart } : {}),
          ...(product.kitPart ? { kitPart: product.kitPart } : {}),
        };
        if (!existing.has(targetKey(target))) {
          existing.add(targetKey(target));
          pending.push(target);
        }
      }
    }
    return pending;
  }, [partNumbers, storeNumbers, targets, ui.category, ui.products, ui.settings.locale, ui.stores]);

  const hasCompleteSelection = storeNumbers.length > 0 && partNumbers.length > 0;
  const canAdd = pendingTargets.length > 0 && !isAdding;
  const duplicateCount = storeNumbers.length * partNumbers.length - pendingTargets.length;

  async function onAdd() {
    if (!canAdd) return;
    setIsAdding(true);
    try {
      if (await setTargets([...targets, ...pendingTargets])) setPartNumbers([]);
    } finally {
      setIsAdding(false);
    }
  }

  async function onRemove(target: Target) {
    await setTargets(targets.filter((item) => targetKey(item) !== targetKey(target)));
  }

  return (
    <TooltipProvider delayDuration={180}>
      <div className="app-canvas min-h-screen text-foreground">
        <main className="flex min-h-screen w-full max-w-none flex-col gap-4 px-5 py-5 lg:h-screen lg:overflow-hidden">
          <header className="surface-panel flex shrink-0 items-center justify-between gap-5 px-5 py-4">
            <div className="flex min-w-0 items-center gap-3.5">
              <div className="brand-mark" aria-hidden="true">
                <Radar className="size-5" strokeWidth={1.8} />
              </div>
              <div className="min-w-0">
                <div className="flex items-center gap-2">
                  <h1 className="truncate text-lg font-semibold tracking-[-0.02em]">
                    果到雷达
                  </h1>
                  <Badge variant="outline" className="hidden border-primary/20 bg-primary/8 text-primary sm:inline-flex">
                    LIVE
                  </Badge>
                  <span className="hidden text-[11px] tabular-nums text-muted-foreground sm:inline">
                    v{packageInfo.version}
                  </span>
                </div>
                <p className="mt-0.5 truncate text-sm text-muted-foreground">
                  Apple 直营店取货库存监控
                </p>
              </div>
            </div>

            <div className="hidden min-w-0 flex-1 items-center justify-center gap-2 min-[860px]:flex">
              <span className="truncate text-sm text-muted-foreground">
                本软件已开源，欢迎下载最新版体验
              </span>
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant="ghost"
                    size="icon-sm"
                    className="rounded-lg"
                    aria-label="在 GitHub 查看开源项目并下载最新版"
                    onClick={() => void openProjectPage()}
                  >
                    <GithubBrandIcon />
                  </Button>
                </TooltipTrigger>
                <TooltipContent>GitHub 项目与最新版下载</TooltipContent>
              </Tooltip>
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant="ghost"
                    size="sm"
                    className="rounded-lg px-2 text-sm text-muted-foreground hover:text-foreground"
                    aria-label="在 X 关注 @suversal，获取更多信息"
                    onClick={() => void openAuthorPage()}
                  >
                    <XBrandIcon />
                    @suversal
                  </Button>
                </TooltipTrigger>
                <TooltipContent>关注我获取更多信息</TooltipContent>
              </Tooltip>
            </div>

            <div className="flex shrink-0 items-center gap-2.5">
              <div className="status-pill" role="status" aria-live="polite">
                <span
                  className={`size-2 rounded-full ${ui.running ? "bg-in-stock shadow-[0_0_12px_var(--in-stock)]" : "bg-muted-foreground/55"}`}
                  aria-hidden="true"
                />
                <span className="hidden text-xs text-muted-foreground sm:inline">监控状态</span>
                <span className="text-sm font-medium">{ui.running ? runningLabel : "已暂停"}</span>
              </div>
              {ui.running ? (
                <Button
                  variant="secondary"
                  className="h-10 rounded-xl px-4"
                  onClick={() => void stopWatching()}
                >
                  <Pause aria-hidden="true" /> 暂停
                </Button>
              ) : (
                <Button
                  className="h-10 rounded-xl px-4 shadow-[0_8px_24px_-12px_var(--primary)]"
                  onClick={() => void startWatching()}
                  disabled={ui.rows.length === 0}
                >
                  <Play aria-hidden="true" /> 开始监控
                </Button>
              )}
            </div>
          </header>

          {ui.trouble !== null && (
            <Alert variant="destructive" className="shrink-0 rounded-2xl border-destructive/25 bg-destructive/8 px-4 py-3">
              <AlertTriangle aria-hidden="true" />
              <AlertTitle>监控结果暂不可信</AlertTitle>
              <AlertDescription>
                {ui.trouble.reason}
                <span>列表状态可能不代表真实库存，请先排查后再继续等待。</span>
                {ui.trouble.advice !== null && (
                  <span className="font-medium">{describeAdvice(ui.trouble.advice)}</span>
                )}
              </AlertDescription>
            </Alert>
          )}

          {ui.update !== null && (
            <Alert className="shrink-0 rounded-2xl border-primary/20 bg-primary/8 px-4 py-3">
              <Download aria-hidden="true" />
              <AlertTitle>{ui.updateInstalled ? "更新已安装" : "发现新版本"} {ui.update.version}</AlertTitle>
              <AlertDescription>
                <span>{ui.updateInstalled ? "请退出并重新打开应用，新版本才会生效。" : `当前版本 ${ui.update.currentVersion}，安装后需重启应用。`}</span>
                {ui.updateProgress !== null && (
                  <div className="w-full space-y-1" role="status" aria-live="polite">
                    <span>{describeUpdateProgress(ui.updateProgress)}</span>
                    {ui.updateProgress.phase === "downloading" && (
                      <progress className="block h-2 w-full max-w-sm accent-primary" aria-label="更新下载进度"
                        max={100} value={updatePercent(ui.updateProgress)} />
                    )}
                  </div>
                )}
                {ui.updateError !== null && <p className="break-words text-destructive" role="alert">{ui.updateError}</p>}
                <div className="mt-1.5 flex items-center gap-2">
                  <Button size="sm" disabled={ui.installing || ui.updateInstalled} onClick={() => void installUpdate()}>
                    {ui.updateInstalled ? "已安装" : ui.installing ? "正在更新…" : ui.updateError ? "重试" : "下载并安装"}
                  </Button>
                  {ui.updateError !== null && (
                    <Button size="sm" variant="outline" onClick={() => void openReleasePage()}>下载安装包</Button>
                  )}
                  <Button size="sm" variant="ghost" disabled={ui.installing} onClick={dismissUpdate}>
                    <X aria-hidden="true" /> 稍后
                  </Button>
                </div>
              </AlertDescription>
            </Alert>
          )}

          <div className="grid min-h-0 flex-1 gap-4 min-[980px]:grid-cols-[minmax(0,1fr)_22rem]">
            <div className="flex min-h-0 flex-col gap-4">
              <section className="surface-panel shrink-0 p-4" aria-labelledby="create-monitor-title">
                <div className="mb-4 flex items-start justify-between gap-4">
                  <div>
                    <div className="eyebrow">
                      <Plus className="size-3.5" aria-hidden="true" /> 新建监控
                    </div>
                    <h2 id="create-monitor-title" className="mt-1 text-base font-semibold tracking-tight">
                      选择想要追踪的门店与型号
                    </h2>
                  </div>
                  {hasCompleteSelection && (
                    <Badge className="border-primary/20 bg-primary/10 text-primary" variant="outline">
                      {storeNumbers.length} × {partNumbers.length}
                    </Badge>
                  )}
                </div>

                <div className="grid grid-cols-2 items-start gap-3 lg:grid-cols-[0.9fr_0.9fr_1.3fr_2fr]">
                  <div className="field-group">
                    <Label className="control-label">
                      <MapPin className="size-3.5" aria-hidden="true" /> 地区
                    </Label>
                    <Combobox
                      className="control-surface w-full"
                      options={ui.regions.map((region) => ({ value: region.locale, label: region.title }))}
                      value={ui.settings.locale}
                      onChange={(locale) => {
                        setStoreNumbers([]);
                        setPartNumbers([]);
                        void changeLocale(locale);
                      }}
                      placeholder="选择地区"
                      searchPlaceholder="搜索地区…"
                      emptyText="没有匹配的地区"
                      disabled={isAdding}
                    />
                  </div>

                  <div className="field-group">
                    <Label className="control-label">
                      <Smartphone className="size-3.5" aria-hidden="true" /> 品类
                    </Label>
                    <Combobox
                      className="control-surface w-full"
                      options={ui.categories.map((category) => ({ value: category.value, label: category.title }))}
                      value={ui.category}
                      onChange={(value) => {
                        setPartNumbers([]);
                        setCategory(value as Category);
                      }}
                      placeholder="选择品类"
                      searchPlaceholder="搜索品类…"
                      emptyText="没有匹配的品类"
                      disabled={isAdding || ui.categories.length === 0}
                    />
                  </div>

                  <div className="field-group">
                    <Label className="control-label">
                      <MapPin className="size-3.5" aria-hidden="true" /> 门店
                      <span className="font-normal text-muted-foreground">可多选</span>
                    </Label>
                    <MultiCombobox
                      key={`stores-${ui.settings.locale}`}
                      className="control-surface w-full"
                      options={storeOptions}
                      values={storeNumbers}
                      onChange={setStoreNumbers}
                      placeholder="选择自提门店"
                      searchPlaceholder="搜索门店…"
                      emptyText="没有匹配的门店"
                      selectionUnit="家门店"
                      disabled={isAdding || storeOptions.length === 0}
                    />
                  </div>

                  <div className="field-group">
                    <Label className="control-label">
                      <PackageCheck className="size-3.5" aria-hidden="true" /> 型号
                      <span className="font-normal text-muted-foreground">可多选</span>
                    </Label>
                    <MultiCombobox
                      key={`products-${ui.settings.locale}-${ui.category}`}
                      className="control-surface w-full"
                      options={productOptions}
                      values={partNumbers}
                      onChange={setPartNumbers}
                      placeholder="选择型号"
                      searchPlaceholder="搜索型号…"
                      emptyText="没有匹配的型号"
                      selectionUnit="个型号"
                      disabled={isAdding || productOptions.length === 0}
                    />
                    <p className="text-[11px] leading-4 text-muted-foreground">
                      单次查询最多包含 20 个零件号，超过后会自动分批，不会遗漏已选型号。
                      {ui.category === "watch"
                        ? " Watch 会先使用目录默认表带查询；也可在监控列表中按官网款式、颜色和尺码选择精确表带。"
                        : ""}
                    </p>
                  </div>
                </div>

                <div className="mt-3 flex flex-wrap items-center justify-between gap-3 border-t border-border/55 pt-3">
                  <p className="min-w-0 flex-1 text-xs leading-5 text-muted-foreground" role="status">
                    {hasCompleteSelection
                      ? `将新增 ${pendingTargets.length} 条监控${duplicateCount > 0 ? `，跳过 ${duplicateCount} 条已有组合` : ""}`
                      : "选择门店和型号后，系统会按全部组合创建监控。"}
                  </p>
                  <div className="flex items-center gap-2">
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button
                          variant="outline"
                          size="icon-lg"
                          className="rounded-xl border-border/70 bg-background/40"
                          aria-label="从 Apple 官网更新当前品类的型号列表"
                          disabled={ui.refreshing}
                          onClick={() => void refreshProducts()}
                        >
                          <RefreshCw className={ui.refreshing ? "animate-spin" : undefined} />
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent>从 Apple 官网更新当前品类的型号列表</TooltipContent>
                    </Tooltip>
                    <Button
                      className="h-10 min-w-28 rounded-xl px-4"
                      onClick={() => void onAdd()}
                      disabled={!canAdd}
                    >
                      <Plus aria-hidden="true" />
                      {isAdding
                        ? "添加中…"
                        : canAdd
                          ? `添加 ${pendingTargets.length} 项`
                          : hasCompleteSelection
                            ? "已在列表中"
                            : "添加监控"}
                    </Button>
                  </div>
                </div>
              </section>

              <section className="surface-panel flex min-h-[280px] flex-1 flex-col overflow-hidden" aria-labelledby="monitor-list-title">
                <div className="flex shrink-0 items-center justify-between border-b border-border/60 px-4 py-3.5">
                  <div className="flex items-center gap-3">
                    <div className="section-icon" aria-hidden="true">
                      <Activity className="size-4" />
                    </div>
                    <div>
                      <h2 id="monitor-list-title" className="text-sm font-semibold">监控列表</h2>
                      <p className="mt-0.5 text-xs text-muted-foreground">新款优先 · 取货与送货状态实时更新</p>
                    </div>
                  </div>
                  <span className="rounded-full border border-border/60 bg-background/40 px-2.5 py-1 text-xs tabular-nums text-muted-foreground">
                    {ui.rows.length} 项
                  </span>
                </div>

                <ScrollArea className="min-h-0 flex-1">
                  <Table className="min-w-[1024px] table-fixed">
                    <TableHeader className="sticky top-0 z-10 bg-card/95">
                      <TableRow className="hover:bg-transparent">
                        <TableHead className="w-36 px-4 text-xs text-muted-foreground">取货状态</TableHead>
                        <TableHead className="w-44 px-4 text-xs text-muted-foreground">门店</TableHead>
                        <TableHead className="px-4 text-xs text-muted-foreground">型号</TableHead>
                        <TableHead className="w-48 px-3 text-xs text-muted-foreground">
                          <span className="flex items-center gap-1.5"><Truck className="size-3.5" aria-hidden="true" />预计送货</span>
                        </TableHead>
                        <TableHead className="w-20 px-2 text-xs text-muted-foreground">最后检查</TableHead>
                        <TableHead className="w-20 px-1 text-xs text-muted-foreground">Bark</TableHead>
                        <TableHead className="sticky right-0 z-20 w-12 border-l border-border/40 bg-card/95" aria-label="操作" />
                      </TableRow>
                    </TableHeader>
                    <TableBody>
                      {ui.rows.length === 0 ? (
                        <TableRow className="hover:bg-transparent">
                          <TableCell colSpan={7} className="h-44 text-center">
                            <div className="mx-auto flex max-w-xs flex-col items-center">
                              <div className="mb-3 flex size-11 items-center justify-center rounded-2xl border border-border/60 bg-muted/35 text-muted-foreground">
                                <Radar className="size-5" aria-hidden="true" />
                              </div>
                              <p className="text-sm font-medium text-foreground">
                                {ui.ready ? "还没有监控项目" : "正在载入目录…"}
                              </p>
                              <p className="mt-1 text-xs leading-5 text-muted-foreground">
                                {ui.ready ? "从上方选择门店和型号，添加后即可开始监控。" : "正在连接本地监控引擎，请稍候。"}
                              </p>
                            </div>
                          </TableCell>
                        </TableRow>
                      ) : (
                        sortedRows.map((row) => (
                          <TableRow key={targetKey(row.target)} className="group hover:bg-muted/22">
                            <TableCell className="px-4"><StatusBadge availability={row.availability} pickupDetails={row.pickupDetails} /></TableCell>
                            <TableCell className="truncate px-4 font-medium" title={row.target.storeTitle}>{row.target.storeTitle}</TableCell>
                            <TableCell className="min-w-0 px-4 py-2.5 text-muted-foreground" title={row.target.productName}>
                              <button
                                className="block max-w-full whitespace-normal text-left leading-5 hover:text-primary hover:underline"
                                aria-label={`打开商品页：${row.target.productName}`}
                                onClick={() => void openTargetProduct(row.target)}
                              >
                                {compactProductName(row.target.productName)}
                              </button>
                              {row.target.companionPart && row.target.kitPart ? (
                                <WatchBandPicker
                                  target={row.target}
                                  targets={targets}
                                  defaultCompanionPart={ui.products.find((product) => product.partNumber === row.target.partNumber)?.companionPart}
                                  disabled={isAdding}
                                />
                              ) : null}
                            </TableCell>
                            <TableCell className="overflow-hidden px-2">
                              <DeliveryBadge pickupDetails={row.pickupDetails} lastCheckedMs={row.lastCheckedMs} />
                            </TableCell>
                            <TableCell className="px-2 font-mono text-xs tabular-nums text-muted-foreground">
                              {formatTime(row.lastCheckedMs)}
                            </TableCell>
                            <TableCell className="px-1">
                              <ProductBarkRoute
                                target={row.target}
                                customUrl={ui.settings.productBarkUrls[row.target.partNumber]}
                                disabled={isAdding}
                              />
                            </TableCell>
                            <TableCell className="sticky right-0 z-10 border-l border-border/40 bg-card/95 pr-2 group-hover:bg-muted">
                              <Button
                                variant="ghost"
                                size="icon-sm"
                                className="text-muted-foreground opacity-80 hover:bg-destructive/10 hover:text-destructive group-hover:opacity-100"
                                aria-label="删除这条监控"
                                disabled={isAdding}
                                onClick={() => void onRemove(row.target)}
                              >
                                <Trash2 />
                              </Button>
                            </TableCell>
                          </TableRow>
                        ))
                      )}
                    </TableBody>
                  </Table>
                </ScrollArea>
              </section>
            </div>

            <aside className="flex min-h-0 flex-col gap-4">
              <section className="grid grid-cols-4 gap-2" aria-label="监控概览">
                <div className="metric-tile">
                  <Radar className="size-4 text-primary" aria-hidden="true" />
                  <span className="metric-value">{ui.rows.length}</span>
                  <span className="metric-label">监控</span>
                </div>
                <div className="metric-tile">
                  <PackageCheck className="size-4 text-in-stock" aria-hidden="true" />
                  <span className="metric-value text-in-stock">{summary.inStock}</span>
                  <span className="metric-label">有货</span>
                </div>
                <div className="metric-tile">
                  <PackageX className="size-4 text-muted-foreground" aria-hidden="true" />
                  <span className="metric-value">{summary.outOfStock}</span>
                  <span className="metric-label">不可取货</span>
                </div>
                <div className="metric-tile">
                  <AlertTriangle className={`size-4 ${summary.untrusted > 0 ? "text-unknown" : "text-muted-foreground"}`} aria-hidden="true" />
                  <span className={`metric-value ${summary.untrusted > 0 ? "text-unknown" : ""}`}>{summary.untrusted}</span>
                  <span className="metric-label">未确认</span>
                </div>
              </section>

              <section className="surface-panel shrink-0 p-4" aria-labelledby="preferences-title">
                <div className="mb-4 flex items-center gap-3">
                  <div className="section-icon" aria-hidden="true"><Settings2 className="size-4" /></div>
                  <div>
                    <h2 id="preferences-title" className="text-sm font-semibold">监控设置</h2>
                    <p className="mt-0.5 text-xs text-muted-foreground">查询频率与提醒方式</p>
                  </div>
                </div>

                <div className="field-group">
                  <Label htmlFor="interval" className="control-label">
                    <Clock3 className="size-3.5" aria-hidden="true" /> 查询间隔
                  </Label>
                  <div className="relative">
                    <Input
                      id="interval"
                      type="number"
                      min={5}
                      className="control-surface select-text pr-12 tabular-nums"
                      value={intervalValue}
                      onChange={(event) => setIntervalDraft(event.target.valueAsNumber)}
                      onBlur={() => {
                        const seconds = Number.isFinite(intervalValue) ? Math.round(intervalValue) : 30;
                        setIntervalDraft(null);
                        void setIntervalSeconds(seconds);
                      }}
                    />
                    <span className="pointer-events-none absolute inset-y-0 right-3 flex items-center text-xs text-muted-foreground">秒</span>
                  </div>
                </div>

                <div className="mt-3 field-group">
                  <div className="flex items-center justify-between gap-3">
                    <Label className="control-label">
                      <MapPin className="size-3.5" aria-hidden="true" /> 送货地区
                    </Label>
                    <span className="text-[10px] text-muted-foreground" role="status" aria-live="polite">
                      {deliverySaving
                        ? "保存中…"
                        : deliveryOptionsLoading
                        ? "读取官网…"
                        : deliveryDirty
                          ? deliverySelectionValid ? "即将保存" : "继续选择"
                          : ui.settings.deliveryRegion ? "已生效" : "未设置"}
                    </span>
                  </div>
                  <div className="grid grid-cols-3 gap-2">
                    <Select
                      value={deliveryOptions.states.some((option) => option.value === deliveryDraft.state) ? deliveryDraft.state : ""}
                      disabled={deliveryOptionsLoading || deliveryOptions.states.length === 0}
                      onValueChange={(value) => {
                        setDeliveryDirty(true);
                        setDeliveryError(null);
                        setDeliveryDraft({ state: value, city: "", district: "" });
                        void refreshDeliveryOptions(value);
                      }}
                    >
                      <SelectTrigger aria-label="省份" className="control-surface h-9 w-full min-w-0 px-2 text-xs">
                        <SelectValue placeholder="省份" />
                      </SelectTrigger>
                      <SelectContent>
                        {deliveryOptions.states.map((option) => (
                          <SelectItem key={option.value} value={option.value}>{option.text}</SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                    <Select
                      value={deliveryOptions.cities.some((option) => option.value === deliveryDraft.city) ? deliveryDraft.city : ""}
                      disabled={deliveryOptionsLoading || !deliveryDraft.state || deliveryOptions.cities.length === 0}
                      onValueChange={(value) => {
                        setDeliveryDirty(true);
                        setDeliveryError(null);
                        setDeliveryDraft((previous) => ({ ...previous, city: value, district: "" }));
                        void refreshDeliveryOptions(deliveryDraft.state, value);
                      }}
                    >
                      <SelectTrigger aria-label="城市" className="control-surface h-9 w-full min-w-0 px-2 text-xs">
                        <SelectValue placeholder="城市" />
                      </SelectTrigger>
                      <SelectContent>
                        {deliveryOptions.cities.map((option) => (
                          <SelectItem key={option.value} value={option.value}>{option.text}</SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                    <Select
                      value={deliveryOptions.districts.some((option) => option.value === deliveryDraft.district) ? deliveryDraft.district : ""}
                      disabled={deliveryOptionsLoading || !deliveryDraft.city || deliveryOptions.districts.length === 0}
                      onValueChange={(value) => {
                        const next = { ...deliveryDraft, district: value };
                        setDeliveryDirty(true);
                        setDeliveryError(null);
                        setDeliveryDraft(next);
                        void persistDeliveryRegion(next);
                      }}
                    >
                      <SelectTrigger aria-label="区" className="control-surface h-9 w-full min-w-0 px-2 text-xs">
                        <SelectValue placeholder="区" />
                      </SelectTrigger>
                      <SelectContent>
                        {deliveryOptions.districts.map((option) => (
                          <SelectItem key={option.value} value={option.value}>{option.text}</SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                  </div>
                  <div className="flex items-start justify-between gap-2">
                    <p className="text-[11px] leading-4 text-muted-foreground">
                      选完“区”后自动保存；{ui.running ? "下轮监控会更新送货日期。" : "启动监控后会查询送货日期。"}
                    </p>
                    <Button
                      type="button"
                      variant="ghost"
                      size="sm"
                      className="h-8 shrink-0 rounded-lg px-2 text-muted-foreground"
                      disabled={deliverySaving || (!ui.settings.deliveryRegion && !deliveryDirty)}
                      onClick={async () => {
                        setDeliverySaving(true);
                        const saved = await saveSettings({ deliveryRegion: null });
                        if (saved) {
                          setDeliveryDraft({ state: "", city: "", district: "" });
                          setDeliveryDirty(false);
                          setDeliveryError(null);
                        } else {
                          setDeliveryError("清除送货地区失败，请稍后重试。");
                        }
                        setDeliverySaving(false);
                      }}
                    >
                      清除
                    </Button>
                  </div>
                  {deliveryError ? (
                    <p role="alert" className="text-[11px] leading-4 text-destructive">{deliveryError}</p>
                  ) : null}
                </div>

                <div className="mt-3 field-group">
                  <Label htmlFor="bark" className="control-label">
                    <BellRing className="size-3.5" aria-hidden="true" /> 默认 Bark 推送
                  </Label>
                  <Input
                    id="bark"
                    className="control-surface select-text"
                    placeholder="https://api.day.app/你的BarkKey"
                    value={barkValue}
                    onChange={(event) => setBarkDraft(event.target.value)}
                    onBlur={() => {
                      setBarkDraft(null);
                      void saveSettings({ barkUrl: barkValue.trim() });
                    }}
                  />
                  <p className="text-xs leading-5 text-muted-foreground">
                    监控列表可为某个型号指定其他人的 Bark 地址。
                  </p>
                </div>

                <div className="mt-4 space-y-1 rounded-xl border border-border/55 bg-background/30 p-1">
                  <div className="setting-row">
                    <span className="flex items-center gap-2 text-sm"><Volume2 className="size-4 text-muted-foreground" aria-hidden="true" />提示音</span>
                    <Switch
                      id="sound"
                      aria-label="提示音"
                      checked={ui.settings.soundEnabled}
                      onCheckedChange={(value) => void saveSettings({ soundEnabled: value })}
                    />
                  </div>
                  <div className="setting-row gap-3">
                    <Label htmlFor="open-on-hit" className="flex items-center gap-2 text-sm font-normal">
                      <ShoppingBag className="size-4 text-muted-foreground" aria-hidden="true" />到货后打开
                    </Label>
                    <Select
                      value={ui.settings.openOnHit}
                      onValueChange={(value) =>
                        void saveSettings({ openOnHit: value as OpenOnHit })
                      }
                    >
                      <SelectTrigger id="open-on-hit" className="h-9 w-[8.5rem] bg-background/45" aria-label="到货后打开">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent align="end">
                        <SelectItem value="none">不自动打开</SelectItem>
                        <SelectItem value="bag">购物袋</SelectItem>
                        <SelectItem value="product">商品详情</SelectItem>
                      </SelectContent>
                    </Select>
                  </div>
                </div>

                <Button variant="outline" className="mt-3 h-10 w-full rounded-xl border-border/70 bg-background/30" onClick={() => void testNotify()}>
                  <BellRing aria-hidden="true" /> 测试提醒与跳转
                </Button>
              </section>

              <section className="surface-panel flex min-h-[150px] flex-1 flex-col overflow-hidden" aria-labelledby="activity-log-title">
                <div className="flex shrink-0 items-center justify-between border-b border-border/60 px-4 py-3">
                  <div className="flex items-center gap-2.5">
                    <SquareTerminal className="size-4 text-muted-foreground" aria-hidden="true" />
                    <h2 id="activity-log-title" className="text-sm font-semibold">活动日志</h2>
                  </div>
                  <span className="text-[11px] tabular-nums text-muted-foreground">{ui.logs.length} 条</span>
                </div>
                <ScrollArea className="min-h-0 flex-1 p-3.5">
                  <pre className="font-mono text-[11px] leading-[1.65] whitespace-pre-wrap text-muted-foreground select-text">
                    {ui.logs.length === 0 ? "等待监控活动…" : ui.logs.join("\n")}
                  </pre>
                </ScrollArea>
              </section>
            </aside>
          </div>
        </main>
      </div>
    </TooltipProvider>
  );
}
