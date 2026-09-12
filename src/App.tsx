import { useEffect, useMemo, useState, useSyncExternalStore } from "react";
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
import { ScrollArea } from "@/components/ui/scroll-area";
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
  openReleasePage,
  openTargetProduct,
  refreshProducts,
  saveSettings,
  setCategory,
  setIntervalSeconds,
  setTargets,
  startWatching,
  stopWatching,
  testNotify,
  watcherStore,
} from "@/lib/store";
import {
  type Availability,
  type PickupDetails,
  type Category,
  describeAdvice,
  formatTime,
  isUntrusted,
  type StatusTone,
  type Target,
  targetKey,
} from "@/lib/types";

import { describeMonitorStatus } from "@/lib/monitorLog";
import { compareNewestProducts, sortMonitorsNewestFirst } from "@/lib/productOrder";

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
  const badge = (
    <Badge
      variant="outline"
      className={`h-7 min-w-20 justify-center gap-2 px-2.5 font-medium ${TONE_CLASS[tone]}`}
    >
      <span className={`size-1.5 rounded-full ${TONE_DOT[tone]}`} aria-hidden="true" />
      {label}
    </Badge>
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

  const barkValue = barkDraft ?? ui.settings.barkUrl;
  const intervalValue = intervalDraft ?? ui.settings.intervalSeconds;
  const latestCheckedMs = Math.max(0, ...ui.rows.map((row) => row.lastCheckedMs ?? 0));
  const secondsUntilNextCheck = latestCheckedMs
    ? Math.max(0, Math.ceil((latestCheckedMs + ui.settings.intervalSeconds * 1_000 - clockMs) / 1_000))
    : null;
  const runningLabel =
    secondsUntilNextCheck === null || secondsUntilNextCheck === 0
      ? "正在查询"
      : `约 ${secondsUntilNextCheck} 秒后检查`;

  const storeOptions = useMemo(
    () => ui.stores.map((store) => ({ value: store.number, label: store.title })),
    [ui.stores],
  );
  const productOptions = useMemo(
    () =>
      ui.products
        .filter((product) => product.category === ui.category)
        .sort(compareNewestProducts)
        .map((product) => ({ value: product.partNumber, label: product.title })),
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
        <main className="mx-auto flex min-h-screen w-full max-w-[1180px] flex-col gap-4 px-5 py-5 lg:h-screen lg:overflow-hidden">
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
                </div>
                <p className="mt-0.5 truncate text-sm text-muted-foreground">
                  Apple 直营店取货库存监控
                </p>
              </div>
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

          <div className="grid min-h-0 flex-1 gap-4 min-[980px]:grid-cols-[minmax(0,1.65fr)_20rem]">
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

                <div className="grid grid-cols-2 gap-3 lg:grid-cols-[0.9fr_0.9fr_1.3fr_2fr]">
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
                      <p className="mt-0.5 text-xs text-muted-foreground">新款优先 · 库存变化实时更新</p>
                    </div>
                  </div>
                  <span className="rounded-full border border-border/60 bg-background/40 px-2.5 py-1 text-xs tabular-nums text-muted-foreground">
                    {ui.rows.length} 项
                  </span>
                </div>

                <ScrollArea className="min-h-0 flex-1">
                  <Table>
                    <TableHeader className="sticky top-0 z-10 bg-card/95">
                      <TableRow className="hover:bg-transparent">
                        <TableHead className="w-28 px-4 text-xs text-muted-foreground">状态</TableHead>
                        <TableHead className="px-3 text-xs text-muted-foreground">门店</TableHead>
                        <TableHead className="px-3 text-xs text-muted-foreground">型号</TableHead>
                        <TableHead className="w-24 px-3 text-xs text-muted-foreground">最后检查</TableHead>
                        <TableHead className="w-14" />
                      </TableRow>
                    </TableHeader>
                    <TableBody>
                      {ui.rows.length === 0 ? (
                        <TableRow className="hover:bg-transparent">
                          <TableCell colSpan={5} className="h-44 text-center">
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
                          <TableRow key={targetKey(row.target)} className="group h-14 hover:bg-muted/22">
                            <TableCell className="px-4"><StatusBadge availability={row.availability} pickupDetails={row.pickupDetails} /></TableCell>
                            <TableCell className="px-3 font-medium">{row.target.storeTitle}</TableCell>
                            <TableCell className="max-w-[24rem] truncate px-3 text-muted-foreground" title={row.target.productName}>
                              <button
                                className="text-left hover:text-primary hover:underline"
                                aria-label={`打开商品页：${row.target.productName}`}
                                onClick={() => void openTargetProduct(row.target)}
                              >
                                {row.target.productName}
                              </button>
                            </TableCell>
                            <TableCell className="px-3 font-mono text-xs tabular-nums text-muted-foreground">
                              {formatTime(row.lastCheckedMs)}
                            </TableCell>
                            <TableCell className="pr-3">
                              <Button
                                variant="ghost"
                                size="icon-sm"
                                className="text-muted-foreground opacity-60 hover:bg-destructive/10 hover:text-destructive group-hover:opacity-100"
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
                  <Label htmlFor="bark" className="control-label">
                    <BellRing className="size-3.5" aria-hidden="true" /> Bark 推送
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
                  <div className="setting-row">
                    <span className="flex items-center gap-2 text-sm"><ShoppingBag className="size-4 text-muted-foreground" aria-hidden="true" />自动打开商品页</span>
                    <Switch
                      id="openbag"
                      aria-label="有货时自动打开商品页"
                      checked={ui.settings.openBagOnHit}
                      onCheckedChange={(value) => void saveSettings({ openBagOnHit: value })}
                    />
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
