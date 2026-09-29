import { useState, type ReactNode } from "react";
import { ChevronDown } from "lucide-react";

const STORAGE_PREFIX = "apw.panel.";

/** 折叠状态只是本机界面偏好；存储不可用时退回默认展开状态。 */
function readOpen(id: string, fallback: boolean): boolean {
  try {
    const value = localStorage.getItem(STORAGE_PREFIX + id);
    return value === null ? fallback : value === "1";
  } catch {
    return fallback;
  }
}

function writeOpen(id: string, open: boolean): void {
  try {
    localStorage.setItem(STORAGE_PREFIX + id, open ? "1" : "0");
  } catch {
    // 私密窗口或存储被禁用时只影响记忆，不影响折叠本身。
  }
}

interface CollapsiblePanelProps {
  id: string;
  icon: ReactNode;
  title: string;
  subtitle: string;
  /** 折叠后在标题下显示的当前状态摘要。 */
  summary: ReactNode;
  children: ReactNode;
}

/** 侧栏可折叠面板。折叠后只保留标题和摘要，把空间让给活动日志。 */
export function CollapsiblePanel({ id, icon, title, subtitle, summary, children }: CollapsiblePanelProps) {
  const [open, setOpen] = useState(() => readOpen(id, true));
  const titleId = `${id}-title`;
  const bodyId = `${id}-body`;

  return (
    <section className="surface-panel min-w-0 shrink-0" aria-labelledby={titleId}>
      {/* 标准折叠面板结构：标题包住按钮，按钮内只放行内元素。 */}
      <h2 id={titleId} className="m-0">
        <button
          type="button"
          className="flex w-full min-w-0 items-center gap-3 rounded-2xl p-4 text-left transition-colors hover:bg-muted/25 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
          aria-expanded={open}
          aria-controls={bodyId}
          onClick={() => {
            setOpen(!open);
            writeOpen(id, !open);
          }}
        >
          <span className="section-icon" aria-hidden="true">{icon}</span>
          <span className="flex min-w-0 flex-1 flex-col">
            <span className="text-sm font-semibold">{title}</span>
            <span className="mt-0.5 truncate text-xs font-normal text-muted-foreground">{open ? subtitle : summary}</span>
          </span>
          <ChevronDown
            className={`size-4 shrink-0 text-muted-foreground transition-transform duration-200 ${open ? "rotate-180" : ""}`}
            aria-hidden="true"
          />
        </button>
      </h2>
      {open ? (
        <div id={bodyId} className="min-w-0 px-4 pb-4">
          {children}
        </div>
      ) : null}
    </section>
  );
}
