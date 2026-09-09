# 果到雷达（Apple Store Inventory Monitor）

[![CI](https://github.com/suversal/apple-store-inventory-monitor/actions/workflows/ci.yml/badge.svg)](https://github.com/suversal/apple-store-inventory-monitor/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/suversal/apple-store-inventory-monitor?display_name=tag)](https://github.com/suversal/apple-store-inventory-monitor/releases/latest)
[![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-blue.svg)](LICENSE)

果到雷达是一款 Apple 直营店到店取货库存监控工具。选好地区、门店和具体型号后，它会定时检查库存；检测到有货时，可以播放提示音、发送系统通知、推送 Bark，并打开 Apple 购物袋页面。

英文项目名为 **Apple Store Inventory Monitor**，仓库与安装包使用
`apple-store-inventory-monitor`。项目使用 Rust、Tauri 2 和 React 编写，支持
macOS、Windows 和 Linux。当前源码版本为 `1.0.0`。

本项目在 [ENCHIGO/apple-pickup-watcher](https://github.com/ENCHIGO/apple-pickup-watcher)
`v0.3.2` 的代码基础上继续开发。上游项目本身是
[hteen/apple-store-helper](https://github.com/hteen/apple-store-helper) 的 GPL-3.0
重写版本。完整的来源、版权和修改记录见 [`NOTICE`](NOTICE)。

> 本项目不是 Apple 官方软件，与 Apple Inc. 没有关系，也没有获得 Apple 授权。它只负责查询和提醒，不能预留库存，也不会替你下单。

## 相比直接上游，我们做了什么

下面只比较本项目与直接上游 `ENCHIGO/apple-pickup-watcher v0.3.2` 的差异。Rust、Tauri、三态库存、四个产品品类、系统托盘和应用内更新等能力已经由上游完成，不算作本项目新增。

| 项目 | 直接上游 v0.3.2 | 果到雷达 v1.0.0 |
| --- | --- | --- |
| Apple 查询会话 | 普通 HTTP 客户端先访问商品页获取 Cookie，再查询库存 | 启动独立的临时 Chrome 或 Edge 会话，在 Apple 页面环境中完成校验和库存请求；遇到 `403` 或 `541` 时会销毁会话并在下一轮重建 |
| 批量添加监控 | 每次选择一家门店和一个型号 | 门店、型号都可以多选，一次生成全部组合；重复组合会自动跳过 |
| 持续有货提醒 | 只在状态从非有货变成有货时提醒一次 | 每轮确认有货都会重新执行提醒，适合库存短暂出现、第一次提醒被系统隐藏等情况 |
| 运行过程反馈 | 主要记录状态变化，长时间无变化时不容易判断是否仍在查询 | 每轮显示开始时间、覆盖的门店和监控项、逐项结果、耗时与下一轮时间；日志最多保留 300 行，避免长时间运行后界面越来越卡 |
| 操作反馈 | 有货时会执行提醒并尝试打开购物袋，但界面不显示具体执行结果 | 明确记录系统通知、提示音、Bark 和自动打开购物袋是否执行；购物袋打开失败也会在日志中说明 |

这些调整主要解决两个实际问题：Apple 的库存接口现在依赖完整浏览器环境，而多门店、多型号监控也需要更清楚的轮次反馈。完整的逐项修改记录和派生关系见 [`NOTICE`](NOTICE)。

## 第一次使用，先看这里

如果你只想使用软件，不需要安装 Rust、Node.js 或 pnpm。它们只供开发者从源码构建。

1. 打开 [Releases](https://github.com/suversal/apple-store-inventory-monitor/releases)，按自己的电脑下载安装包。下载页为空，表示当前还没有公开发布的安装包。
2. 确认电脑已安装 Google Chrome 或 Microsoft Edge。Linux 也可以使用 PATH 中的 Chromium。软件不会读取你平时使用的浏览器资料。
3. 安装并打开果到雷达。macOS 和 Windows 可能会拦截未签名的应用，处理方法见[安装](#安装)。
4. 选择地区和品类，再勾选门店与型号，点击「添加监控」。
5. 先点一次「测试提醒」，确认系统通知、声音或 Bark 能正常收到。
6. 点击「开始监控」。看到「未知」时先看活动日志，不要把它当成无货。

最容易误解的一点是：这个工具不能保证你一定买到商品。Apple 可能限流、调整接口，库存也可能在提醒后立即变化。重要商品请同时对照 Apple 官网。

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
- 活动日志会在每一轮逐项显示有货、无货或异常，不把结果藏在汇总数字里。
- 支持应用内检查更新。

## 为什么会有「未知」状态

库存查询失败和无货是两回事。

| 状态 | 含义 | 是否提醒 |
| --- | --- | --- |
| 待查询 | 刚添加，或还没有开始第一轮查询 | 否 |
| 有货 | Apple 明确返回该门店可取货 | 是 |
| 无货 | Apple 明确返回该门店不可取货 | 否 |
| 未知 | 请求被拦截、超时、限流，或响应结构无法识别 | 显示原因，不冒充无货 |

旧版工具最危险的问题不是接口报错，而是把报错后的空结果继续显示成无货。界面看起来一直在刷新，实际已经拿不到库存。果到雷达把「未知」保留到界面和日志中，用户能直接看到本轮结果是否可信。

## Apple 查询会话

Apple 当前商品页会先完成浏览器环境校验，再请求库存接口。普通 HTTP 客户端即使带上常见请求头和购物袋 Cookie，也可能收到 `HTTP 541`。

果到雷达会启动一个独立的无界面 Chrome 或 Edge：

1. 创建临时浏览器资料目录。
2. 打开当前地区的 Apple 正式购买页，等待页面校验完成。
3. 在同一页面和同一会话中请求库存。
4. 多个门店串行查询，任意两次请求至少间隔 2 秒。
5. 如果会话被 `403` 或 `541` 拒绝，销毁当前会话，下轮重新建立。

临时浏览器不会读取用户日常使用的 Chrome/Edge 个人资料、Cookie 或浏览记录。应用退出后，临时进程和资料目录会一起清理。

## 运行要求

运行时需要以下任意一种浏览器：

- Google Chrome
- Microsoft Edge
- Chromium（Linux，需要能从 PATH 直接启动）

应用会自动查找常见安装位置。没有找到时，监控项会显示为「未知」，日志会说明缺少浏览器。

目前只提供桌面版，不支持 iPhone、iPad 或 Android。Bark 只是把有货消息推送到 iPhone，真正的库存监控仍在电脑上运行。电脑休眠、关机或彻底退出应用后，监控也会停止。

## 安装

从 [GitHub Releases](https://github.com/suversal/apple-store-inventory-monitor/releases) 下载对应系统的安装包。

| 你的电脑 | 推荐下载 |
| --- | --- |
| Apple 芯片 Mac（M1、M2、M3、M4 等） | 文件名含 `aarch64` 的 `.dmg` |
| Intel 芯片 Mac | 文件名含 `x64` 的 `.dmg` |
| 64 位 Windows | `*-setup.exe` |
| Ubuntu 24.04 或更新的 64 位 Linux | `*_amd64.deb` 或 `*_amd64.AppImage` |

不知道 Mac 使用哪种芯片时，点击屏幕左上角苹果菜单，选择「关于本机」：显示 Apple M 系列就下载 `aarch64`，显示 Intel 就下载 `x64`。

### macOS

1. 打开 `.dmg`，把 `Apple Store Inventory Monitor.app` 拖进「应用程序」。
2. 第一次打开时，根据系统提示授予通知权限；否则有货时可能看不到系统通知。
3. 安装包暂未经过 Apple 公证。如果系统提示无法验证开发者，先到「系统设置 → 隐私与安全性」确认被拦截的是本应用，再选择「仍要打开」。

如果系统仍提示应用已损坏，并且你确认安装包来自本仓库的 Release，可以执行：

```bash
xattr -cr "/Applications/Apple Store Inventory Monitor.app"
```

然后重新打开应用。这条命令只移除该应用的下载隔离标记，不会关闭 macOS 的全局安全功能。不要对来源不明的应用执行这条命令。

### Windows

推荐使用 `*-setup.exe`。未签名版本可能触发 SmartScreen。确认文件来自本仓库 Release 后，可点击「更多信息」，再选择「仍要运行」。Windows 自带 Microsoft Edge，一般不需要另装浏览器。

### Linux

Release 提供 x64 的 `.deb` 和 `.AppImage`。安装包在 Ubuntu 24.04 上构建，需要 glibc 2.39 或更高版本；较老的发行版请从源码构建。

AppImage 首次运行前需要增加执行权限：

```bash
chmod +x ./*.AppImage
```

Linux 还需要自行安装 Chrome、Edge 或 Chromium。使用 Chromium 时，请确认终端可以运行 `chromium` 或 `chromium-browser`。

## 使用方法

### 添加监控

1. 选择地区和品类。
2. 在门店下拉框中勾选一家或多家门店。
3. 在型号下拉框中搜索并勾选一个或多个具体配置。
4. 点击「添加监控」。
5. 点击右上角「开始监控」。

门店和型号会按组合添加。例如，选择 2 家门店和 3 个型号，会生成 6 条监控。已存在的组合会被跳过。

切换地区时，当前还没有添加的门店和型号选择会被清空；已经在监控列表里的项目不会被删除。找不到刚发布的型号时，点击「添加监控」旁边的刷新按钮，从 Apple 官网更新当前品类的型号列表。刷新失败不会清空内置列表。

### 查看结果

监控列表中的「最后检查」表示该条目最近一次完成查询的时间。活动日志会在每轮结束后显示类似内容：

```text
第 2 轮 · 无货：上海-香港广场 Apple Watch 42 毫米…
第 2 轮 · 无货：上海-五角场 Apple Watch 42 毫米…
第 2 轮 · 有货：上海-环球港 iPhone 17…
第 2 轮查询完成（耗时 4.4 秒）；约 30 秒后开始下一轮。
```

第一条商品有货不会停止本轮查询。监控引擎会继续处理剩余门店和型号；持续有货时，
每一轮都会再次执行已启用的提醒动作。

如果启用了「自动打开购物袋」，持续有货也会每轮再次打开页面。只想收到通知、不想反复出现浏览器标签页时，请关闭这个选项。

监控期间可以关闭主窗口，应用会留在系统托盘继续运行。不要让电脑进入睡眠，也不要从托盘菜单彻底退出，否则查询会停止。

### 查询间隔

默认间隔为 30 秒。界面允许的下限是 5 秒，但长时间使用过短间隔更容易触发 Apple 风控，也不会明显提高实际抢购成功率。日常使用建议保留 30 秒或更长。

### 提醒方式

- 系统通知：由桌面系统显示。
- 提示音：在应用内开关。
- Bark：填写完整 Bark 地址后启用，留空即关闭。
- 自动打开购物袋：检测到有货后调用系统默认浏览器。

「测试提醒」会测试当前提醒配置，但不会发起库存查询。

#### Bark 怎么配置

Bark 是一款 iPhone 推送工具，不使用 Bark 可以跳过这一段。

1. 在 iPhone 安装并打开 Bark。
2. 复制 Bark 首页显示的完整推送地址，通常形如 `https://api.day.app/你的Key`。
3. 把整段地址粘贴到果到雷达的「Bark 推送」输入框，点击页面其他位置让设置保存。
4. 点击「测试提醒」。手机收到消息后再开始监控。

Bark 地址相当于推送凭证，不要截图发到公开 issue，也不要提交到 Git 仓库。Bark 失败不会改变库存状态，活动日志会单独说明哪个提醒渠道失败。

### 应用更新

应用启动后会静默检查 GitHub Releases。发现新版本时，界面会显示版本号和「下载并安装」按钮；更新不会在后台自动安装。

macOS 和 Windows 安装包目前没有操作系统层面的开发者签名，但自动更新包使用本项目独立的 Tauri 更新密钥签名。这两件事不是一回事：前者关系到 Gatekeeper 或 SmartScreen 提示，后者用于阻止应用安装被篡改的更新包。

## 设置与数据

新包使用独立的应用标识 `com.suversal.apple-store-inventory-monitor` 和配置目录。
首次启动时会从改名前的 `apple-pickup-watcher` 目录读取并复制现有设置；旧文件不会
被修改或删除，因此回退旧包也不会丢失监控列表。

| 系统 | 设置文件 |
| --- | --- |
| macOS | `~/Library/Application Support/apple-store-inventory-monitor/settings.v2.json` |
| Windows | `%APPDATA%\apple-store-inventory-monitor\settings.v2.json` |
| Linux | `$XDG_CONFIG_HOME/apple-store-inventory-monitor/settings.v2.json` |

设置文件包含监控目标、查询间隔和提醒选项。Bark 地址也会保存在本机，请不要将该文件上传到公开 issue 或提交进 Git 仓库。

项目没有接入分析统计或崩溃上报。正常运行时会访问 Apple 官网；启用 Bark 时会访问用户填写的 Bark 服务；检查更新时会访问本仓库的 GitHub Releases。

## 常见问题

### Releases 页面没有可以下载的安装包

这通常表示正式版本还在构建，或 Release 仍是草稿。请等待当前版本发布，不要安装 issue、网盘或陌生账号提供的同名文件。会使用开发工具的用户可以按[从源码运行](#从源码运行)自行构建。

### 启动开发版时报 `failed to run cargo metadata`

终端没有找到 Rust 工具链。先载入 Cargo 环境：

```bash
source "$HOME/.cargo/env"
pnpm tauri dev
```

如果 `~/.cargo/bin/cargo` 不存在，需要先安装 Rust。

### 源码目录改名后，构建日志仍指向旧目录

Cargo 的 `target` 目录可能保留了旧项目的绝对路径。典型报错是当前正在构建 `apple-store-inventory-monitor`，却去另一个旧目录读取 Tauri 的 `permissions` 文件。

在仓库根目录执行：

```bash
cargo clean
pnpm tauri dev
```

`cargo clean` 只会清理当前项目的 Rust 编译产物，不会删除源码或用户设置。如果日志仍指向旧目录，再检查终端是否设置了 `CARGO_TARGET_DIR`，以及 `.cargo/config.toml` 中是否写死了旧路径。

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

### 测试提醒没有系统通知

先检查系统是否允许 `Apple Store Inventory Monitor` 发送通知，再确认勿扰模式或专注模式没有隐藏通知。提示音、系统通知和 Bark 是三个独立渠道，其中一个失败不会阻止另外两个，也不会影响库存判断。

### 关闭窗口后应用没有退出

这是预期行为。关闭窗口会收进系统托盘，监控继续运行。需要彻底退出时，在托盘菜单中选择「退出」。

### 电脑锁屏后还会监控吗

只锁屏通常不会停止应用，但电脑进入睡眠、关机或应用被彻底退出后不会继续查询。长时间监控时，请接通电源并按自己的需要调整系统睡眠设置。

## 从源码运行

### 工具链

- Rust 1.90 或更高版本
- Node.js 当前 LTS（至少满足 Vite 7 的 Node.js 20.19+ 要求）
- pnpm 9 或更高版本
- 对应平台的 Tauri 2 系统依赖
- Chrome 或 Edge，用于真实库存查询

macOS 需要 Xcode Command Line Tools：

```bash
xcode-select --install
```

Windows 需要 Microsoft C++ Build Tools，并在安装器里选择「使用 C++ 的桌面开发」。还需要 WebView2 Runtime，Windows 11 通常已经自带。

Debian/Ubuntu 构建依赖：

```bash
sudo apt update
sudo apt install -y \
  libwebkit2gtk-4.1-dev libgtk-3-dev build-essential curl wget file \
  libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev \
  libasound2-dev patchelf xdg-utils
```

其他 Linux 发行版的包名不同，请对照 [Tauri 2 前置依赖文档](https://v2.tauri.app/start/prerequisites/)。

### 开发命令

```bash
git clone https://github.com/suversal/apple-store-inventory-monitor.git
cd apple-store-inventory-monitor
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
cargo test -p apple-store-inventory-monitor \
  'chromium_fetcher::tests::真实chromium会话连续检查四家门店两轮' \
  -- --ignored --nocapture
```

这条测试会复用同一浏览器会话，连续检查上海四家门店两轮。它用于验证页面校验、会话复用、请求节流、响应解析，以及第一家门店有货后仍继续查询其他门店。

### 构建应用

本地构建但不生成发布签名：

```bash
pnpm tauri build --no-sign
```

产物位于 `target/release/bundle/`。自动更新包需要仓库维护者配置 `TAURI_SIGNING_PRIVATE_KEY`；私钥设置了密码时还要配置 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`。没有私钥时不能生成可被现有客户端验证的更新签名。

正式版本由 `.github/workflows/release.yml` 构建。推送 `v*` 标签后，GitHub Actions 会生成 macOS、Windows 和 Linux 安装包，并先创建草稿 Release；确认四个平台全部成功后再公开发布，避免用户下载到缺少部分平台或 `latest.json` 尚未完整生成的版本。

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

本仓库直接基于 [ENCHIGO/apple-pickup-watcher](https://github.com/ENCHIGO/apple-pickup-watcher)
`v0.3.2` 继续开发，并保留了对应 Git 历史。主要后续改动包括多门店与多型号组合监控、逐项轮次日志、重复提醒、临时 Chromium 查询会话、独立应用标识，以及「果到雷达」这一对外名称。

`ENCHIGO/apple-pickup-watcher` 是 [hteen/apple-store-helper](https://github.com/hteen/apple-store-helper)
的 GPL-3.0 重写版本。因此，本项目既保留直接上游 ENCHIGO 的贡献记录，也继续遵守最初项目的 GPL-3.0 派生要求。版权、承继资源和逐项修改说明见 [`NOTICE`](NOTICE)。

本项目按 [GNU General Public License v3.0 or later](LICENSE) 发布。分发修改版本时，需要继续遵守 GPL-3.0 的源码和许可要求。

## 反馈问题

请在本仓库的 [Issues](https://github.com/suversal/apple-store-inventory-monitor/issues) 提交问题，并尽量附上：

- 操作系统与芯片架构，例如 macOS 26 / Apple M4；
- 应用版本；
- 地区、门店和商品品类；
- 活动日志中的完整错误文字；
- 问题能否稳定复现。

提交前请删掉 Bark 地址、设备 Key、个人路径及其他隐私信息。库存随时会变化，单独一张「有货」或「无货」截图通常不足以定位问题。
