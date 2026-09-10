export interface UpdateProgress {
  phase: "checking" | "downloading" | "verifying" | "installing";
  downloaded: number;
  total: number | null;
}

export function updatePercent(progress: UpdateProgress): number | undefined {
  if (progress.phase !== "downloading" || !progress.total || progress.total <= 0) return undefined;
  return Math.min(100, Math.max(0, Math.floor(progress.downloaded / progress.total * 100)));
}

export function describeUpdateProgress(progress: UpdateProgress): string {
  if (progress.phase === "checking") return "正在确认更新版本…";
  if (progress.phase === "verifying") return "下载完成，正在验证安装包…";
  if (progress.phase === "installing") return "验证通过，正在安装…";
  const mb = (progress.downloaded / 1024 / 1024).toFixed(1);
  const percent = updatePercent(progress);
  return percent === undefined ? `已下载 ${mb} MB，正在下载…`
    : `正在下载 ${percent}%（${mb} / ${(progress.total! / 1024 / 1024).toFixed(1)} MB）`;
}

export function describeUpdateError(error: unknown): string {
  const message = String(error);
  if (message.includes("different key")) {
    return "更新签名密钥不匹配，已停止安装。请从项目 Releases 页面下载完整安装包，退出应用后替换旧版；重复点击下载无法解决。";
  }
  if (/signature|verification/i.test(message)) {
    return "安装包签名验证失败，已停止安装。请重试下载，或从项目 Releases 页面获取完整安装包。";
  }
  if (/timeout|timed out|超时/i.test(message)) return "更新连接超时，请检查网络后重试。";
  return `更新失败：${message}`;
}
