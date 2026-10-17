import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { saveCheckoutSettings } from "@/lib/store";
import type { BrowserRefreshSettings as RefreshSettings } from "@/lib/types";

export function BrowserRefreshSettings({ settings }: { settings: RefreshSettings }) {
  const [enabled, setEnabled] = useState(settings.enabled);
  const [seconds, setSeconds] = useState(String(settings.intervalSeconds));
  const [url, setUrl] = useState(settings.url);
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState("");
  useEffect(() => {
    setEnabled(settings.enabled);
    setSeconds(String(settings.intervalSeconds));
    setUrl(settings.url);
  }, [settings.enabled, settings.intervalSeconds, settings.url]);

  const apply = async () => {
    const intervalSeconds = Number(seconds);
    if (!Number.isInteger(intervalSeconds) || intervalSeconds < 60 || intervalSeconds > 86400) {
      setMessage("间隔请输入 60～86400 之间的整数秒数。");
      return;
    }
    setSaving(true);
    try {
      const saved = await saveCheckoutSettings({ browserRefresh: { enabled, intervalSeconds, url: url.trim() } });
      setMessage(saved ? (enabled ? "已启用；一个完整间隔后开始尝试刷新，结果见活动日志。" : "已关闭定时刷新。") : "保存失败，请查看活动日志中的原因。");
    } finally {
      setSaving(false);
    }
  };

  return (
    <fieldset disabled={saving} className="grid min-w-0 gap-3 border-t border-border/60 pt-3">
      <legend className="px-1 text-sm font-medium">下单浏览器定时刷新</legend>
      <div className="setting-row">
        <Label htmlFor="browser-refresh-enabled" className="text-sm font-normal">定时刷新购物袋</Label>
        <Switch id="browser-refresh-enabled" checked={enabled} onCheckedChange={setEnabled} />
      </div>
      <div className="field-group">
        <Label htmlFor="browser-refresh-interval" className="control-label">刷新间隔（秒）</Label>
        <Input id="browser-refresh-interval" className="control-surface" type="number" min={60} max={86400} step={1}
          value={seconds} onChange={(event) => setSeconds(event.target.value)} aria-describedby="browser-refresh-help" />
      </div>
      <div className="field-group">
        <Label htmlFor="browser-refresh-url" className="control-label">固定购物袋 URL（可留空）</Label>
        <Input id="browser-refresh-url" className="control-surface select-text" type="url" autoComplete="off"
          placeholder="https://www.apple.com.cn/shop/bag" value={url} onChange={(event) => setUrl(event.target.value)}
          aria-describedby="browser-refresh-help" />
      </div>
      <p id="browser-refresh-help" className="text-[11px] leading-4 break-words text-muted-foreground">
        默认 300 秒（5 分钟）。先手动打开上面的下单浏览器并登录，再回到购物袋。
        留空刷新该标签的当前购物袋；填写后每轮打开固定的 HTTPS Apple 购物袋网址。
        登录、结账、付款页面和未提交的输入会跳过，自动结账期间也不刷新。
        关闭下单浏览器后不会自动打开；收进托盘后继续，退出应用后停止。
        此功能仅帮助保持页面活跃，不能保证 Apple 登录一直有效。
      </p>
      <Button type="button" variant="outline" disabled={saving} onClick={() => void apply()}>
        {saving ? "保存中…" : "保存刷新设置"}
      </Button>
      {message ? <p role="status" className="text-[11px] leading-4 text-muted-foreground">{message}</p> : null}
    </fieldset>
  );
}
