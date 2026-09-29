import { useCallback, useEffect, useState } from "react";
import { Check, Copy, FileCode2, Globe, LoaderCircle, PlugZap, RefreshCw } from "lucide-react";
import { Combobox } from "@/components/Combobox";
import { CollapsiblePanel } from "@/components/CollapsiblePanel";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { listClashNodes, saveSettings, selectClashNode, testClashRoute } from "@/lib/store";
import type {
  ClashRouteCheck,
  ClashSettings,
  NetworkMode,
  NetworkSettings as Network,
  RouteList,
} from "@/lib/types";

const MODE_HINTS: Record<NetworkMode, string> = {
  system: "跟随系统代理。开着 Clash 系统代理或 TUN 时，出口由 Clash 规则决定。",
  pinned: "经 Clash 专用端口固定使用下方指定的节点，不自动切换；选 DIRECT 即用本机网络，开着 TUN 也有效。",
  clash: "只走专用端口；节点被 Apple 拒绝时冷却并换下一个，不影响电脑本身的节点。",
};

const MODE_LABELS: Record<NetworkMode, string> = {
  system: "跟随系统代理",
  pinned: "指定节点",
  clash: "Clash 节点轮换",
};

/** Clash Verge「全局扩展脚本」模板，与 README 中的脚本保持一致。 */
const SCRIPT_TEMPLATE = "// Clash Verge「全局扩展脚本」：节点只供果到雷达的专用端口使用，不影响电脑本身的节点\nconst APW_PREFIX = \"APW·\";\nconst APW_GROUP = __GROUP__;\nconst APW_SUBSCRIPTIONS = {\n  机场1: \"机场订阅地址\",\n};\n\nfunction main(config, profileName) {\n  const providers = config[\"proxy-providers\"] || {};\n  const used = [];\n  for (const [label, url] of Object.entries(APW_SUBSCRIPTIONS)) {\n    if (!/^https?:\\/\\//.test(url)) continue;\n    const key = `apw-${label}`;\n    providers[key] = {\n      type: \"http\",\n      url,\n      interval: 86400,\n      path: `./providers/${key}.yaml`,\n      override: { \"additional-prefix\": `${APW_PREFIX}${label}|` },\n      \"health-check\": { enable: false },\n    };\n    used.push(key);\n  }\n  config[\"proxy-providers\"] = providers;\n\n  // 原有策略组若自动收录全部节点（include-all / include-all-providers），排除专用节点\n  const guard = `^${APW_PREFIX}`;\n  const groups = (config[\"proxy-groups\"] || []).filter((g) => g.name !== APW_GROUP);\n  for (const group of groups) {\n    if (group[\"include-all\"] || group[\"include-all-providers\"]) {\n      const old = group[\"exclude-filter\"];\n      if (old && old.includes(guard)) continue;\n      group[\"exclude-filter\"] = old ? `(?:${old})|${guard}` : guard;\n    }\n  }\n  groups.push({ name: APW_GROUP, type: \"select\", proxies: [\"DIRECT\"], use: used });\n  config[\"proxy-groups\"] = groups;\n\n  const listeners = (config.listeners || []).filter((l) => l.name !== \"apple-store-monitor\");\n  listeners.push({ name: \"apple-store-monitor\", type: \"mixed\", port: __PORT__, listen: \"127.0.0.1\", proxy: APW_GROUP });\n  config.listeners = listeners;\n  return config;\n}";

function configSnippet(clash: ClashSettings): string {
  return SCRIPT_TEMPLATE.replace("__GROUP__", JSON.stringify(clash.group || "果到雷达")).replace(
    "__PORT__",
    String(clash.proxyPort),
  );
}

function ClashScriptDialog({ clash }: { clash: ClashSettings }) {
  const [copied, setCopied] = useState(false);
  const script = configSnippet(clash);
  return (
    <Dialog onOpenChange={() => setCopied(false)}>
      <DialogTrigger asChild>
        <Button type="button" variant="ghost" className="h-9 rounded-xl text-muted-foreground">
          <FileCode2 aria-hidden="true" /> Clash 配置脚本
        </Button>
      </DialogTrigger>
      <DialogContent className="max-h-[85vh] grid-rows-[auto_minmax(0,1fr)_auto] sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>Clash 配置脚本</DialogTitle>
          <DialogDescription className="leading-5">
            在 Clash Verge 订阅页右键「全局扩展脚本」→ 编辑文件，粘贴后在 APW_SUBSCRIPTIONS 中填写机场订阅地址。
            不要使用「全局扩展覆写配置」，它不支持追加策略组。脚本只新增专用策略组和端口，电脑本身的节点保持不变；同时需要开启外部控制器。
          </DialogDescription>
        </DialogHeader>
        <pre className="min-h-0 overflow-auto rounded-lg border border-border/60 bg-muted/30 p-3 font-mono text-xs leading-5 select-text">
          {script}
        </pre>
        <div className="flex justify-end">
          <Button
            type="button"
            variant="outline"
            className="rounded-xl"
            onClick={async () => {
              try {
                await navigator.clipboard.writeText(script);
                setCopied(true);
              } catch {
                setCopied(false);
              }
            }}
          >
            {copied ? <Check aria-hidden="true" /> : <Copy aria-hidden="true" />}
            {copied ? "已复制" : "复制脚本"}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

/** 从每轮日志的线路说明中取出节点名，例如「Clash 节点 香港01（本轮切换 1 次）」。 */
function nodeFromRoute(route: string | null): string | null {
  const match = route?.match(/^Clash 节点 (.+?)(（本轮切换 \d+ 次）)?$/);
  const node = match?.[1];
  return node && node !== "尚未选定" ? node : null;
}

function nodeLabel(name: string, delay: number | null, resting: boolean): string {
  const display = name.replace(/^APW·/, "");
  const status = name === "DIRECT" ? "家宽直连" : delay === null ? "测速超时" : `${delay} ms`;
  return `${display} · ${status}${resting ? " · 冷却中" : ""}`;
}

function NodePicker({
  route,
  pinned,
  onPin,
}: {
  route: string | null;
  /** 指定节点模式下固定使用的节点；轮换模式为 null。 */
  pinned: string | null;
  onPin: (node: string) => Promise<void>;
}) {
  const [list, setList] = useState<RouteList | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [switching, setSwitching] = useState(false);
  const [current, setCurrent] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    const result = await listClashNodes();
    if (typeof result === "string") {
      setError(result);
    } else {
      setError(null);
      setList(result);
      setCurrent(result.now);
    }
    setLoading(false);
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // 自动切换后跟上每轮实际使用的节点。
  const routeNode = nodeFromRoute(route);
  useEffect(() => {
    if (routeNode) setCurrent(routeNode);
  }, [routeNode]);
  const value = pinned ?? current;

  const options = (list?.nodes ?? []).map((node) => ({
    value: node.name,
    label: nodeLabel(node.name, node.delay, node.resting),
  }));
  const reachable = list?.nodes.filter((node) => node.name === "DIRECT" || node.delay !== null).length ?? 0;

  return (
    <div className="field-group">
      <div className="flex items-center justify-between gap-2">
        <Label className="control-label">{pinned !== null ? "指定节点" : "当前节点"}</Label>
        <span className="text-[10px] text-muted-foreground">
          {loading ? "测速中…" : list ? `${reachable}/${list.nodes.length} 个可用` : ""}
        </span>
      </div>
      <div className="grid min-w-0 grid-cols-[minmax(0,1fr)_auto] gap-2">
        <Combobox
          className="control-surface w-full min-w-0"
          options={options}
          value={value ?? ""}
          placeholder={loading ? "正在读取节点…" : "选择节点"}
          searchPlaceholder="搜索节点，如 香港"
          emptyText="没有匹配的节点"
          disabled={switching || options.length === 0}
          onChange={async (node) => {
            if (node === value) return;
            setSwitching(true);
            if (pinned !== null) {
              await onPin(node);
            } else if (await selectClashNode(node)) {
              setCurrent(node);
            }
            setSwitching(false);
          }}
        />
        <Button
          type="button"
          variant="outline"
          size="icon"
          className="size-10 rounded-xl border-border/70 bg-background/30"
          aria-label="重新测速"
          title="重新测速"
          disabled={loading}
          onClick={() => void refresh()}
        >
          <RefreshCw className={loading ? "animate-spin" : ""} aria-hidden="true" />
        </Button>
      </div>
      <p className="text-[11px] leading-4 break-words text-muted-foreground">
        {error ??
          (pinned !== null
            ? "固定使用该节点，被 Apple 拒绝时不会自动切换，下一轮继续用它重试。"
            : "手动指定后立即生效；该节点之后被 Apple 拒绝或连不通时，仍会自动切换到下一个。")}
      </p>
    </div>
  );
}

function CheckResult({ check, port }: { check: ClashRouteCheck | string | null; port: number }) {
  if (check === null) return null;
  if (typeof check === "string") {
    return <p role="alert" className="text-[11px] leading-4 break-words text-destructive">{check}</p>;
  }
  return (
    <dl className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-1 rounded-lg border border-border/55 bg-background/30 px-3 py-2 text-[11px] leading-4">
      <dt className="text-muted-foreground">内核</dt>
      <dd className="truncate">{check.version}</dd>
      <dt className="text-muted-foreground">可用节点</dt>
      <dd>
        {check.nodes.length} 个{check.skipped > 0 ? `（已排除 ${check.skipped} 个信息节点）` : ""}
      </dd>
      <dt className="text-muted-foreground">测速超时</dt>
      <dd className={check.timeoutCount > 0 ? "text-unknown" : ""}>
        {check.timeoutCount} 个{check.timeoutCount > 0 ? "，切换前会逐个复测并跳过" : ""}
      </dd>
      <dt className="text-muted-foreground">当前节点</dt>
      <dd className="break-words">{check.now ?? "未选择"}</dd>
      <dt className="text-muted-foreground">端口 {port}</dt>
      <dd className={check.portOpen ? "text-in-stock" : "text-destructive"}>
        {check.portOpen ? "可连接" : "无法连接，请检查脚本中的 listeners"}
      </dd>
    </dl>
  );
}

export function NetworkSettings({
  network,
  running,
  route,
}: {
  network: Network;
  running: boolean;
  route: string | null;
}) {
  const [draft, setDraft] = useState<ClashSettings>(network.clash);
  const [checking, setChecking] = useState(false);
  const [check, setCheck] = useState<ClashRouteCheck | string | null>(null);

  useEffect(() => setDraft(network.clash), [network.clash]);

  const saveClash = () => {
    if (JSON.stringify(draft) === JSON.stringify(network.clash)) return;
    setCheck(null);
    void saveSettings({ network: { ...network, clash: draft } });
  };
  const text = (key: "controller" | "secret" | "group" | "nodeFilter") => ({
    value: draft[key],
    onChange: (event: React.ChangeEvent<HTMLInputElement>) => setDraft({ ...draft, [key]: event.target.value }),
    onBlur: saveClash,
  });

  return (
    <CollapsiblePanel
      id="network"
      icon={<Globe className="size-4" />}
      title="查询线路"
      subtitle={route ? `当前：${route}` : "Apple 查询使用的出口 IP"}
      summary={route ?? MODE_LABELS[network.mode]}
    >
      <div className="field-group">
        <Label htmlFor="network-mode" className="control-label">出口方式</Label>
        <Select
          value={network.mode}
          onValueChange={(mode) => {
            setCheck(null);
            void saveSettings({ network: { ...network, mode: mode as NetworkMode } });
          }}
        >
          <SelectTrigger id="network-mode" className="control-surface w-full" aria-label="出口方式">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {(Object.keys(MODE_LABELS) as NetworkMode[]).map((mode) => (
              <SelectItem key={mode} value={mode}>{MODE_LABELS[mode]}</SelectItem>
            ))}
          </SelectContent>
        </Select>
        <p className="text-[11px] leading-4 break-words text-muted-foreground">
          {MODE_HINTS[network.mode]}
          {running ? " 修改后会在下一次查询时重启查询浏览器。" : ""}
        </p>
      </div>

      {network.mode !== "system" ? (
        <div className="mt-3 grid min-w-0 gap-3">
          <NodePicker
            route={route}
            pinned={network.mode === "pinned" ? network.clash.pinnedNode : null}
            onPin={async (node) => {
              // 先保存，保证重启后仍用该节点；再立即切换，不必等到下一轮。
              if (await saveSettings({ network: { ...network, clash: { ...network.clash, pinnedNode: node } } })) {
                await selectClashNode(node);
              }
            }}
          />
          <div className="field-group">
            <Label htmlFor="clash-controller" className="control-label">控制接口</Label>
            <Input
              id="clash-controller"
              className="control-surface select-text"
              placeholder="http://127.0.0.1:9097"
              {...text("controller")}
            />
          </div>
          <div className="grid min-w-0 grid-cols-[minmax(0,1fr)_6rem] gap-2">
            <div className="field-group">
              <Label htmlFor="clash-secret" className="control-label">密钥</Label>
              <Input
                id="clash-secret"
                type="password"
                className="control-surface select-text"
                placeholder="没有可留空"
                {...text("secret")}
              />
            </div>
            <div className="field-group">
              <Label htmlFor="clash-port" className="control-label">专用端口</Label>
              <Input
                id="clash-port"
                inputMode="numeric"
                className="control-surface select-text tabular-nums"
                placeholder="7899"
                value={draft.proxyPort ? String(draft.proxyPort) : ""}
                onChange={(event) => {
                  const port = Number.parseInt(event.target.value.replace(/\D/g, ""), 10);
                  setDraft({ ...draft, proxyPort: Number.isFinite(port) ? Math.min(port, 65535) : 0 });
                }}
                onBlur={saveClash}
              />
            </div>
          </div>
          <div className="grid min-w-0 grid-cols-2 gap-2">
            <div className="field-group">
              <Label htmlFor="clash-group" className="control-label">策略组</Label>
              <Input id="clash-group" className="control-surface select-text" placeholder="果到雷达" {...text("group")} />
            </div>
            <div className="field-group">
              <Label htmlFor="clash-filter" className="control-label">节点关键词</Label>
              <Input
                id="clash-filter"
                className="control-surface select-text"
                placeholder="全部，或 香港|日本"
                {...text("nodeFilter")}
              />
            </div>
          </div>

          <div className="grid grid-cols-2 gap-2">
            <Button
              type="button"
              variant="outline"
              className="h-9 rounded-xl border-border/70 bg-background/30"
              disabled={checking}
              onClick={async () => {
                setChecking(true);
                setCheck(await testClashRoute(draft));
                setChecking(false);
              }}
            >
              {checking ? <LoaderCircle className="animate-spin" aria-hidden="true" /> : <PlugZap aria-hidden="true" />}
              检查线路
            </Button>
            <ClashScriptDialog clash={draft} />
          </div>
          <CheckResult check={check} port={draft.proxyPort} />
        </div>
      ) : null}
    </CollapsiblePanel>
  );
}
