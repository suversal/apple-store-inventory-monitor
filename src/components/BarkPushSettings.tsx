import { useEffect, useState } from "react";
import { BellRing } from "lucide-react";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { saveSettings } from "@/lib/store";
import type { BarkLevel, BarkOptions } from "@/lib/types";

/** 重复提醒窗口/间隔的收敛范围，与 Rust 侧 `BarkOptions::normalize` 保持一致。 */
const REPEAT_WINDOW_MIN_SECONDS = 30;
const REPEAT_WINDOW_MAX_SECONDS = 3600;
const REPEAT_INTERVAL_MIN_SECONDS = 5;
const REPEAT_WINDOW_DEFAULT = 300;
const REPEAT_INTERVAL_DEFAULT = 30;

const LEVEL_LABELS: Record<BarkLevel, string> = {
  default: "不指定",
  active: "active（立即亮屏显示）",
  time_sensitive: "timeSensitive（专注模式也显示）",
  passive: "passive（只加入通知列表，不亮屏）",
  critical: "critical（重要警告，忽略静音与勿扰）",
};

function clampWindow(value: number): number {
  if (!Number.isFinite(value)) return REPEAT_WINDOW_DEFAULT;
  return Math.min(Math.max(Math.round(value), REPEAT_WINDOW_MIN_SECONDS), REPEAT_WINDOW_MAX_SECONDS);
}

function clampInterval(value: number, window: number): number {
  if (!Number.isFinite(value)) return Math.min(REPEAT_INTERVAL_DEFAULT, window);
  return Math.min(Math.max(Math.round(value), REPEAT_INTERVAL_MIN_SECONDS), window);
}

/**
 * Bark 推送增强设置：通知等级、铃声、音量、持续响铃与重复提醒。
 *
 * 这些参数只在 Bark 推送里生效；所有项都可以留空/保持默认，此时请求与旧版
 * 完全一致。重复提醒的发送在后台任务里进行，不会占用查询轮次的时间。
 */
export function BarkPushSettings({ options }: { options: BarkOptions }) {
  const [sound, setSound] = useState(options.sound);
  const [volume, setVolume] = useState(options.volume === null ? "" : String(options.volume));
  const [windowDraft, setWindowDraft] = useState(String(options.repeatSeconds || REPEAT_WINDOW_DEFAULT));
  const [intervalDraft, setIntervalDraft] = useState(
    String(options.repeatIntervalSeconds || REPEAT_INTERVAL_DEFAULT),
  );

  useEffect(() => setSound(options.sound), [options.sound]);
  useEffect(() => setVolume(options.volume === null ? "" : String(options.volume)), [options.volume]);
  useEffect(
    () => setWindowDraft(String(options.repeatSeconds || REPEAT_WINDOW_DEFAULT)),
    [options.repeatSeconds],
  );
  useEffect(
    () => setIntervalDraft(String(options.repeatIntervalSeconds || REPEAT_INTERVAL_DEFAULT)),
    [options.repeatIntervalSeconds],
  );

  const save = (patch: Partial<BarkOptions>) =>
    void saveSettings({ barkOptions: { ...options, ...patch } });

  return (
    <div className="mt-3 space-y-3 rounded-xl border border-border/55 bg-background/30 p-3">
      <p className="flex items-center gap-2 text-xs font-medium text-muted-foreground">
        <BellRing className="size-3.5" aria-hidden="true" /> Bark 推送增强
      </p>

      <div className="field-group">
        <Label htmlFor="bark-level" className="control-label">通知等级</Label>
        <Select value={options.level} onValueChange={(value) => save({ level: value as BarkLevel })}>
          <SelectTrigger id="bark-level" className="control-surface w-full" aria-label="通知等级">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {(Object.keys(LEVEL_LABELS) as BarkLevel[]).map((level) => (
              <SelectItem key={level} value={level}>{LEVEL_LABELS[level]}</SelectItem>
            ))}
          </SelectContent>
        </Select>
        <p className="text-[11px] leading-4 break-words text-muted-foreground">
          「重要警告」需要先在 iPhone 设置与 Bark App 里开启对应权限；开启后会忽略静音和勿扰模式。
        </p>
      </div>

      <div className="grid min-w-0 grid-cols-[minmax(0,1fr)_6rem] gap-2">
        <div className="field-group">
          <Label htmlFor="bark-sound" className="control-label">铃声</Label>
          <Input
            id="bark-sound"
            className="control-surface select-text"
            placeholder="默认铃声"
            value={sound}
            onChange={(event) => setSound(event.target.value)}
            onBlur={() => {
              if (sound.trim() !== options.sound) save({ sound: sound.trim() });
            }}
          />
          <p className="text-[11px] leading-4 break-words text-muted-foreground">
            铃声名，如 alarm、minuet；留空用 Bark 默认。
          </p>
        </div>
        <div className="field-group">
          <Label htmlFor="bark-volume" className="control-label">音量</Label>
          <Input
            id="bark-volume"
            inputMode="numeric"
            className="control-surface select-text tabular-nums"
            placeholder="默认 5"
            value={volume}
            onChange={(event) => setVolume(event.target.value.replace(/\D/g, ""))}
            onBlur={() => {
              if (volume.trim() === "") {
                if (options.volume !== null) save({ volume: null });
                return;
              }
              const parsed = Math.min(Number.parseInt(volume, 10), 10);
              if (parsed !== options.volume) save({ volume: parsed });
              else setVolume(String(parsed));
            }}
          />
          <p className="text-[11px] leading-4 break-words text-muted-foreground">
            0–10，仅「重要警告」生效。
          </p>
        </div>
      </div>

      <div className="setting-row">
        <Label htmlFor="bark-call" className="text-sm font-normal">持续响铃</Label>
        <Switch
          id="bark-call"
          aria-label="持续响铃"
          checked={options.call}
          onCheckedChange={(call) => save({ call })}
        />
      </div>
      <p className="text-[11px] leading-4 break-words text-muted-foreground">
        开启后铃声重复播放约 30 秒；配合「重要警告」可在静音、勿扰模式下持续响铃。
      </p>

      <div className="setting-row">
        <Label htmlFor="bark-repeat" className="text-sm font-normal">到货后重复提醒</Label>
        <Switch
          id="bark-repeat"
          aria-label="到货后重复提醒"
          checked={options.repeatSeconds > 0}
          onCheckedChange={(enabled) =>
            save(
              enabled
                ? {
                    repeatSeconds: options.repeatSeconds || REPEAT_WINDOW_DEFAULT,
                    repeatIntervalSeconds: options.repeatIntervalSeconds || REPEAT_INTERVAL_DEFAULT,
                  }
                : { repeatSeconds: 0, repeatIntervalSeconds: 0 },
            )
          }
        />
      </div>
      {options.repeatSeconds > 0 ? (
        <div className="grid min-w-0 grid-cols-2 gap-2">
          <div className="field-group">
            <Label htmlFor="bark-repeat-window" className="control-label">持续（秒）</Label>
            <Input
              id="bark-repeat-window"
              inputMode="numeric"
              className="control-surface select-text tabular-nums"
              value={windowDraft}
              onChange={(event) => setWindowDraft(event.target.value.replace(/\D/g, ""))}
              onBlur={() => {
                const next = clampWindow(Number.parseInt(windowDraft, 10));
                setWindowDraft(String(next));
                if (next !== options.repeatSeconds) {
                  save({
                    repeatSeconds: next,
                    repeatIntervalSeconds: clampInterval(options.repeatIntervalSeconds, next),
                  });
                }
              }}
            />
          </div>
          <div className="field-group">
            <Label htmlFor="bark-repeat-interval" className="control-label">每隔（秒）</Label>
            <Input
              id="bark-repeat-interval"
              inputMode="numeric"
              className="control-surface select-text tabular-nums"
              value={intervalDraft}
              onChange={(event) => setIntervalDraft(event.target.value.replace(/\D/g, ""))}
              onBlur={() => {
                const next = clampInterval(Number.parseInt(intervalDraft, 10), options.repeatSeconds);
                setIntervalDraft(String(next));
                if (next !== options.repeatIntervalSeconds) save({ repeatIntervalSeconds: next });
              }}
            />
          </div>
        </div>
      ) : null}
      <p className="text-[11px] leading-4 break-words text-muted-foreground">
        {options.repeatSeconds > 0
          ? `检测到有货后，在 ${options.repeatSeconds} 秒内每隔 ${options.repeatIntervalSeconds} 秒再提醒一次；期间重新检测到有货会重新计时。提醒在后台发送，不影响查询。`
          : "开启后按「持续/每隔」参数反复提醒，适合离开电脑时使用；重新检测到有货会重新计时。"}
      </p>
    </div>
  );
}
