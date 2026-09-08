# 果到雷达

果到雷达是一款 Apple 直营店库存监控工具。选择地区、门店和具体型号后，它会定时查询到店取货库存；检测到有货时，可以播放提示音、发送系统通知、推送 Bark，并打开 Apple 购物袋页面。

项目使用 Rust、Tauri 2 和 React 编写，支持 macOS、Windows 和 Linux。当前版本为 `0.3.2`。

> 本项目与 Apple Inc. 没有关系，也没有获得 Apple 授权。它只负责查询和提醒，不会代替用户下单。

## 功能

- 支持 iPhone、iPad、Mac 和 Apple Watch。
- 内置中国大陆、中国香港、中国台湾、日本、新加坡、澳大利亚和马来西亚的 Apple Store 列表。
- 门店和型号都可以多选，添加时会自动生成全部组合。
- 同一门店的多个型号合并查询，减少不必要的请求。
- 库存分为「有货」「无货」「未知」三种状态，查询失败不会被写成无货。
- 支持系统通知、提示音和 Bark 推送。
- 有货时可以自动打开对应地区的 Apple 购物袋页面。
- 窗口关闭后可继续在系统托盘运行。
- 型号目录可以从 Apple 官网刷新；网络失败时仍可使用内嵌目录。
- 活动日志会显示每轮实际检查数量，以及有货、无货和异常数量。
- 支持应用内检查更新。

## 为什么会有「未知」状态

库存查询失败和无货是两回事。

| 状态 | 含义 | 是否提醒 |
| --- | --- | --- |
| 有货 | Apple 明确返回该门店可取货 | 是 |
| 无货 | Apple 明确返回该门店不可取货 | 否 |
| 未知 | 请求被拦截、超时、限流，或响应结构无法识别 | 显示原因，不冒充无货 |

旧版工具最危险的问题不是接口报错，而是把报错后的空结果继续显示成无货。界面看起来一直在刷新，实际已经拿不到库存。果到雷达把「未知」保留到界面和日志中，用户能直接看到本轮结果是否可信。

## Apple 查询会话

Apple 当前商品页会先完成浏览器环境校验，再请求库存接口。普通 HTTP 客户端即使带上常见请求头和购物袋 Cookie，也可能收到 `HTTP 541`。

果到雷达会启动一个独立的无界面 Chrome 或 Edge：

1. 创建临时浏览器资料目录。
2. 打开所监控型号的 Apple 商品页，等待页面校验完成。
3. 在同一页面和同一会话中请求库存。
4. 多个门店串行查询，任意两次请求至少间隔 2 秒。
5. 如果会话被 `403` 或 `541` 拒绝，销毁当前会话，下轮重新建立。

临时浏览器不会读取用户日常使用的 Chrome/Edge 个人资料、Cookie 或浏览记录。应用退出后，临时进程和资料目录会一起清理。

## 运行要求

运行时需要以下任意一种浏览器：

- Google Chrome
- Microsoft Edge

应用会自动查找常见安装位置。没有找到时，监控项会显示为「未知」，日志会说明缺少浏览器。

## 安装

从 [GitHub Releases](https://github.com/ENCHIGO/apple-pickup-watcher/releases) 下载对应系统的安装包。

### macOS

安装包暂未经过 Apple 公证。将 `果到雷达.app` 拖入「应用程序」后，如果系统提示应用损坏或无法验证开发者，执行：

```bash
xattr -cr "/Applications/果到雷达.app"
```

然后重新打开应用。

### Windows

推荐使用 `*-setup.exe`。未签名版本可能触发 SmartScreen，确认文件来自本仓库 Release 后，可在「更多信息」中选择继续运行。

### Linux

Release 提供 `.deb` 和 `.AppImage`。AppImage 首次运行前需要增加执行权限：

```bash
chmod +x 果到雷达_*.AppImage
```

## 使用方法

### 添加监控

1. 选择地区和品类。
2. 在门店下拉框中勾选一家或多家门店。
3. 在型号下拉框中搜索并勾选一个或多个具体配置。
4. 点击「添加监控」。
5. 点击右上角「开始监控」。

门店和型号会按组合添加。例如，选择 2 家门店和 3 个型号，会生成 6 条监控。已存在的组合会被跳过。

### 查看结果

监控列表中的「最后检查」表示该条目最近一次完成查询的时间。活动日志会在每轮结束后显示类似内容：

```text
本轮已检查 3 项：有货 1 项、无货 2 项、异常 0 项；约 30 秒后开始下一轮。
```

第一条商品有货不会停止本轮查询。监控引擎会继续处理剩余门店和型号，完成后再生成整轮统计。

### 查询间隔

默认间隔为 30 秒。界面允许的下限是 5 秒，但长时间使用过短间隔更容易触发 Apple 风控，也不会明显提高实际抢购成功率。日常使用建议保留 30 秒或更长。

### 提醒方式

- 系统通知：由桌面系统显示。
- 提示音：在应用内开关。
- Bark：填写完整 Bark 地址后启用，留空即关闭。
- 自动打开购物袋：检测到有货后调用系统默认浏览器。

「测试提醒」会测试当前提醒配置，但不会发起库存查询。

## 设置与数据

为了兼容改名前的版本，应用内部标识和设置目录继续使用 `apple-pickup-watcher`。改名或升级不会清空原有监控列表。

| 系统 | 设置文件 |
| --- | --- |
| macOS | `~/Library/Application Support/apple-pickup-watcher/settings.v2.json` |
| Windows | `%APPDATA%\apple-pickup-watcher\settings.v2.json` |
| Linux | `$XDG_CONFIG_HOME/apple-pickup-watcher/settings.v2.json` |

设置文件包含监控目标、查询间隔和提醒选项。Bark 地址也会保存在本机，请不要将该文件上传到公开 issue 或提交进 Git 仓库。

项目没有接入分析统计或崩溃上报。正常运行时会访问 Apple 官网；启用 Bark 时会访问用户填写的 Bark 服务；检查更新时会访问本仓库的 GitHub Releases。

## 常见问题

### 启动开发版时报 `failed to run cargo metadata`

终端没有找到 Rust 工具链。先载入 Cargo 环境：

```bash
source "$HOME/.cargo/env"
pnpm tauri dev
```

如果 `~/.cargo/bin/cargo` 不存在，需要先安装 Rust。

### 出现 HTTP 541

`541` 表示这次请求没有取得可信的库存结果，不代表无货。程序会把对应条目标为「未知」，并在下轮重建浏览器会话。

如果连续多轮仍然失败：

1. 确认 Chrome 或 Edge 可以正常打开 Apple 官网。
2. 暂停监控并重启应用。
3. 保持默认查询间隔，不要连续快速启停。
4. 对照 Apple 官网，并尝试其他网络排除本地网络问题。

### 官网有货，应用却显示无货

先核对门店、容量、颜色和零件号是否完全一致。Apple 的网页可能默认切换到附近门店，也可能显示另一种配置。若应用显示的是「未知」，应先处理日志中的错误，不能把未知当成无货。

### 为什么必须安装 Chrome 或 Edge

库存查询依赖 Apple 商品页完成的浏览器校验。Tauri 自带的 WebView 和普通 HTTP 请求在当前流程下无法稳定取得库存 JSON，因此应用使用独立 Chromium 会话。

### 关闭窗口后应用没有退出

这是预期行为。关闭窗口会收进系统托盘，监控继续运行。需要彻底退出时，在托盘菜单中选择「退出」。

## 从源码运行

### 工具链

- Rust 1.90 或更高版本
- Node.js 当前 LTS
- pnpm 9 或更高版本
- 对应平台的 Tauri 2 系统依赖
- Chrome 或 Edge，用于真实库存查询

macOS 需要 Xcode Command Line Tools：

```bash
xcode-select --install
```

Debian/Ubuntu 构建依赖：

```bash
sudo apt update
sudo apt install -y \
  libwebkit2gtk-4.1-dev build-essential curl wget file \
  libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev \
  libasound2-dev
```

### 开发命令

```bash
git clone https://github.com/ENCHIGO/apple-pickup-watcher.git
cd apple-pickup-watcher
pnpm install
source "$HOME/.cargo/env"
pnpm tauri dev
```

只启动前端页面可以运行 `pnpm dev`，但浏览器页面无法调用 Tauri 命令，不能据此验证真实监控功能。

### 检查与测试

提交前运行：

```bash
source "$HOME/.cargo/env"
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
pnpm build
```

离线测试不会请求 Apple。真实接口回归测试需要本机安装 Chrome/Edge，并会访问 Apple 官网：

```bash
cargo test -p apw-app \
  'chromium_fetcher::tests::真实chromium会话连续检查四家门店两轮' \
  -- --ignored --nocapture
```

这条测试会复用同一浏览器会话，连续检查上海四家门店两轮。它用于验证页面校验、会话复用、请求节流、响应解析，以及第一家门店有货后仍继续查询其他门店。

### 构建应用

本地构建但不生成发布签名：

```bash
pnpm tauri build --no-sign
```

产物位于 `target/release/bundle/`。自动更新包需要仓库维护者配置 `TAURI_SIGNING_PRIVATE_KEY` 和 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`；没有私钥时不能生成可被现有客户端验证的更新签名。

## 代码结构

```text
crates/apw-core/
  apple.rs                HTTP 错误分类与库存响应解析
  apple_catalog.rs        从 Apple 购买页解析型号目录
  catalog.rs              在线目录与内嵌快照
  config.rs               设置保存、迁移与损坏保护
  model.rs                地区、商品、门店和三态库存模型
  notify.rs               Bark 与提示音
  watcher.rs              监控调度、分组查询、事件与退避

src-tauri/
  src/chromium_fetcher.rs  临时 Chromium 会话与真实库存请求
  src/lib.rs               Tauri 命令、托盘、更新和通知装配

src/
  App.tsx                  主界面
  components/              界面组件
  lib/store.ts             前端状态与事件日志
  lib/types.ts             Rust/TypeScript 边界类型
```

库存是否可取货只由 Rust 核心判断。前端负责展示状态，不会根据日志文本或空响应自行推断「无货」。

## 最近一次真实验证

2026-09-08 使用同一个临时 Chromium 会话，对上海以下四家门店连续查询两轮：

- R390 香港广场
- R401 上海环贸 iapm
- R581 五角场
- R683 环球港

8 次查询均返回明确库存状态。真实库存随时会变化，这项验证只说明当时的查询链路和多门店循环正常，不代表这些门店当前仍有货。

## 来源与许可

本项目是 [hteen/apple-store-helper](https://github.com/hteen/apple-store-helper) 的 GPL-3.0 派生重写版本，现用名为「果到雷达」。原项目版权、派生关系和修改说明见 [`NOTICE`](NOTICE)。

本项目按 [GNU General Public License v3.0 or later](LICENSE) 发布。分发修改版本时，需要继续遵守 GPL-3.0 的源码和许可要求。
