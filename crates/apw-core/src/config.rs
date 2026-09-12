//! 用户设置的读写。
//!
//! 这个模块只跟磁盘打交道，不认识网络也不认识界面。
//!
//! # 为什么「读不出来」必须是错误，而不是「返回默认值」
//!
//! 这是整个模块最容易写错、后果也最重的一处。[`SettingsStore::load`] 只在
//! **文件确实不存在**时才回退到默认设置；文件存在但读不动、超限、或解析不了，
//! 一律返回 [`ConfigError`]。
//!
//! 区分这两者不是洁癖：调用方拿到默认设置会照常继续，并在用户改动任何一项时
//! 写盘 —— 而那份盘上原本躺着用户攒了很久的监控列表。如果「读失败」也返回默认值，
//! 程序启动后几毫秒内就会用一份空配置覆盖掉唯一的那份，用户全程看不到任何提示。
//! 正确的处置是：拿到错误 → 放弃写盘 → 用 [`SettingsStore::preserve_corrupted`]
//! 把原文件改名留档 → 告诉用户档案在哪。
//!
//! 这和 [`crate::model::Availability`] 守的是同一条线：**「不知道」不能被折叠成
//! 一个看起来正常的值。**

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::model::{REGIONS, Target, region_by_locale};

/// 检测到有货时自动打开的 Apple 页面。
///
/// 反序列化同时接受旧版的布尔值：`true` 表示购物袋，`false` 表示不自动打开。
/// 这样升级不会悄悄改变现有用户的跳转习惯。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenOnHit {
    None,
    #[default]
    Bag,
    Product,
}

impl<'de> Deserialize<'de> for OpenOnHit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Current {
            None,
            Bag,
            Product,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Legacy(bool),
            Current(Current),
        }

        Ok(match Wire::deserialize(deserializer)? {
            Wire::Legacy(true) | Wire::Current(Current::Bag) => Self::Bag,
            Wire::Legacy(false) | Wire::Current(Current::None) => Self::None,
            Wire::Current(Current::Product) => Self::Product,
        })
    }
}

/// 默认查询间隔（秒）。
///
/// 上游写死 500 毫秒一轮，即每个门店每秒两次请求。这个频率对一个公开的商品
/// 查询接口来说过高，是触发风控的直接原因，也是上游 issue 里 503 / 541 反复
/// 出现的背景。30 秒足够应付抢购，同时不会把自己送进黑名单。
pub const DEFAULT_INTERVAL_SECONDS: u64 = 30;

/// 允许设置的间隔下限（秒）。保留手动调快的余地，但不允许低到必然触发风控。
pub const MIN_INTERVAL_SECONDS: u64 = 5;

/// 读取设置文件时接受的字节上限。
///
/// 一份正常配置只有几百字节，单条 [`Target`] 序列化后不到 200 字节，登记上千个
/// 目标也才几百 KB，1 MiB 是数量级上的余量。之所以必须有上限：Go 版用
/// `os.ReadFile` 整读，磁盘损坏、别的程序写错了文件、或有人刻意构造一个几 GB 的
/// settings.json 时，内存会在任何错误处理之前就被吃光，进程直接被杀 —— 用户连
/// 「配置读不出来」这句提示都看不到，更谈不上留档。
pub const MAX_SETTINGS_BYTES: usize = 1 << 20;

/// 配置目录名，位于系统约定的用户配置目录之下。
///
/// 上游把 `user_settings.json` 直接写在进程工作目录。打包成 macOS `.app` 之后，
/// 工作目录取决于应用被如何启动，可能是 `/` 这种不可写的位置，设置会静默丢失。
const APP_DIR: &str = "apple-store-inventory-monitor";

/// 改名前版本使用的配置目录。只用于首次迁移，不会写入或删除。
const PREVIOUS_APP_DIR: &str = "apple-pickup-watcher";

/// 新版设置文件名。
///
/// **刻意不沿用 Go 版的 `settings.json`。** 两版的字段命名不同（Go 版是
/// snake_case，这里是 camelCase，见 [`Settings`] 的 serde 标注），共用一个文件名
/// 会让两个版本互相把对方的配置读成一堆缺省值再覆盖掉。分成两个文件之后：
/// 新版首次启动可以用 [`SettingsStore::import_legacy`] 把旧配置读过来，
/// 用户想退回 Go 版时那份 `settings.json` 也还原封不动地在。
const SETTINGS_FILE: &str = "settings.v2.json";

/// Go 版留下的设置文件名，只读不写。
const LEGACY_SETTINGS_FILE: &str = "settings.json";

/// 设置读写过程中的失败。
///
/// 变体分得这么细，是因为调用方要据此给出不同的提示：「文件大得离谱」和
/// 「JSON 坏了」对用户意味着完全不同的事情。至于**处置**方式三者是一致的 ——
/// 都属于「读不到旧配置」，都必须放弃写盘并把原文件留档。
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// 系统没有给出用户配置目录的位置。
    #[error("定位用户配置目录失败：系统未提供配置目录位置")]
    NoConfigDir,

    /// 文件系统操作失败。
    #[error("{action} {} 失败：{source}", .path.display())]
    Io {
        /// 正在做的事，用于拼出一句人话，如「打开设置文件」。
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// 设置文件超过 [`MAX_SETTINGS_BYTES`]。
    ///
    /// 单独一个变体，好让调用方能用 `matches!` 把它和「JSON 坏了」区分开。
    #[error("设置文件 {} 超过 {limit} 字节上限", .path.display())]
    TooLarge { path: PathBuf, limit: usize },

    /// 文件读到了，但内容不是一份能解析的设置。
    #[error("解析设置文件 {} 失败：{source}", .path.display())]
    Malformed {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// 把设置序列化成 JSON 失败。
    #[error("序列化设置失败：{source}")]
    Encode {
        #[source]
        source: serde_json::Error,
    },
}

impl ConfigError {
    fn io(action: &'static str, path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            action,
            path: path.into(),
            source,
        }
    }
}

/// 持久化的用户设置。
///
/// 字段名跨 IPC 边界要和前端对齐，所以统一小驼峰，与 [`Target`]、
/// [`crate::model::Product`] 等类型一致。
///
/// 容器级 `#[serde(default)]` 是有意加的：磁盘上的文件可能来自旧版本、少几个
/// 字段。缺字段应当取**默认值**，而不是取该类型的零值 —— 否则一份老配置读上来
/// 会把 `soundEnabled` 变成 `false`，用户什么都没改，提示音却自己关了。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// 上次选择的地区 locale。
    pub locale: String,
    /// 监控目标列表。
    pub targets: Vec<Target>,
    /// 每轮查询之间的间隔秒数。
    pub interval_seconds: u64,
    /// 为空表示不启用 Bark 推送。
    pub bark_url: String,
    /// 按商品零件号覆盖默认 Bark；同一型号在不同门店共用一条路由。
    pub product_bark_urls: BTreeMap<String, String>,
    /// 有货时是否播放提示音。
    pub sound_enabled: bool,
    /// 有货时自动打开的页面；旧字段 `openBagOnHit` 通过别名兼容。
    #[serde(alias = "openBagOnHit")]
    pub open_on_hit: OpenOnHit,
}

/// 内置地区表里的第一个 locale，作为兜底取值。
///
/// 不写 `REGIONS[0]`：那是个会 panic 的下标访问，而这是库代码。
fn fallback_locale() -> &'static str {
    REGIONS.first().map_or("zh_CN", |r| r.locale)
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            locale: fallback_locale().to_string(),
            targets: Vec::new(),
            interval_seconds: DEFAULT_INTERVAL_SECONDS,
            bark_url: String::new(),
            product_bark_urls: BTreeMap::new(),
            sound_enabled: true,
            open_on_hit: OpenOnHit::Bag,
        }
    }
}

impl Settings {
    /// 修正越界或非法的字段，使设置始终可用。
    ///
    /// 这里只做「收敛」，不做「丢弃用户意图」：认不出的 locale 换成默认地区、
    /// 过小的间隔拉回默认值、重复的目标去掉。真正被丢掉的只有结构上就发不出
    /// 请求的目标（地区/门店/零件号为空），它们留着也只会在界面上永远显示未知。
    pub fn normalize(&mut self) {
        if region_by_locale(&self.locale).is_none() {
            self.locale = fallback_locale().to_string();
        }

        // 注意是「小于下限就回到默认值」，不是「夹到下限」。手抖填了 1 秒的人
        // 想要的是快，但 5 秒同样会被风控盯上；退回 30 秒才是安全的那一侧。
        if self.interval_seconds < MIN_INTERVAL_SECONDS {
            self.interval_seconds = DEFAULT_INTERVAL_SECONDS;
        }

        // 去重时**新建 Vec 再整体替换**，不在原 Vec 上就地压缩。
        //
        // Go 版写的是 kept := s.Targets[:0]，复用底层数组原地压缩。而 Save 虽然
        // 按值收 Settings，Targets 却和调用方共用同一块数组：调用方持有
        // [A, A, B] 时，去重结果 [A, B] 会把底层数组改成 [A, B, B]，调用方那个
        // 切片长度仍是 3 —— 内容悄悄从 [A, A, B] 变成了 [A, B, B]，于是
        // 「改设置 → 保存 → 把 targets 交给引擎」这条路径会莫名多出一条重复行。
        //
        // Rust 的 Vec 是独占所有权的，retain 串不到别人身上；真正的防线在
        // [`SettingsStore::save`] 的签名上：它收 &Settings，规范化只发生在自己的
        // 副本上，编译器保证调用方那份数据一个字节都不会被动。
        let mut kept: Vec<Target> = Vec::with_capacity(self.targets.len());
        let mut seen: std::collections::HashSet<crate::model::TargetKey> =
            std::collections::HashSet::with_capacity(self.targets.len());
        for t in &self.targets {
            // 用 trim 判空：手工编辑过的文件里全是空格的字段和空字符串一样发不出请求。
            if t.locale.trim().is_empty()
                || t.store_number.trim().is_empty()
                || t.part_number.trim().is_empty()
            {
                continue;
            }
            if seen.insert(t.key()) {
                kept.push(t.clone());
            }
        }
        self.targets = kept;

        // 型号专属 Bark 只保留仍在监控的型号。地址在保存时统一去掉首尾空白，
        // 空值等价于「沿用默认 Bark」，不必在配置文件里留一条无效覆盖。
        let active_parts: HashSet<String> = self
            .targets
            .iter()
            .map(|target| target.part_number.clone())
            .collect();
        self.product_bark_urls.retain(|part_number, bark_url| {
            *bark_url = bark_url.trim().to_string();
            active_parts.contains(part_number) && !bark_url.is_empty()
        });
    }

    /// 查询间隔。
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_seconds)
    }

    /// 返回某个型号实际使用的 Bark 地址：专属地址优先，否则沿用默认地址。
    pub fn bark_url_for(&self, target: &Target) -> &str {
        self.product_bark_urls
            .get(&target.part_number)
            .map_or(self.bark_url.as_str(), String::as_str)
    }
}

/// 设置文件的读写入口。
///
/// 没有像 Go 版那样带一把互斥锁，因为这里根本不需要：每次写盘都用一个独立命名的
/// 临时文件，最后一步 `rename` 是原子的，并发写入最坏也只是后到的那份胜出，
/// 绝不会出现两份内容交织在一起的文件。反过来说，Go 那把进程内的锁本来也挡不住
/// 第二个进程或第二份安装同时写。
#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: PathBuf,
    previous_path: Option<PathBuf>,
}

impl SettingsStore {
    /// 指向系统用户配置目录下的设置文件。
    ///
    /// **只算路径，不碰磁盘。** 构造一个 Store 是个纯粹的内存操作，不该在用户
    /// 机器上留下痕迹 —— 目录会在 [`save`](Self::save) 真正写盘时才创建。这样
    /// 「程序启动了但用户什么都没设置」的情况下，配置目录始终是干净的。
    pub fn new() -> Result<Self, ConfigError> {
        let dir = dirs::config_dir().ok_or(ConfigError::NoConfigDir)?;
        Ok(Self {
            path: dir.join(APP_DIR).join(SETTINGS_FILE),
            previous_path: Some(dir.join(PREVIOUS_APP_DIR).join(SETTINGS_FILE)),
        })
    }

    /// 指向任意路径，供测试使用。
    pub fn at(path: PathBuf) -> Self {
        Self {
            path,
            previous_path: None,
        }
    }

    /// 设置文件的完整路径，便于在界面上告诉用户配置存在哪。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 首次启动新包时读取改名前版本的 v2 设置。
    ///
    /// 旧文件保持原样，只把解析出的设置交给调用方写入新目录。这样新旧安装包
    /// 拥有完全不同的配置身份，同时用户现有的监控列表和提醒选项不会丢失。
    pub fn import_previous_version(&self) -> Option<Settings> {
        let path = self.previous_path.as_ref()?;
        let file = File::open(path).ok()?;
        let data = read_capped(file, MAX_SETTINGS_BYTES).ok()??;
        let mut settings: Settings = serde_json::from_slice(&data).ok()?;
        settings.normalize();
        Some(settings)
    }

    /// 读取设置。
    ///
    /// **文件不存在**返回默认设置且不算错误；文件存在却读不出来一律返回错误。
    /// 这个区分是刻意的，理由见模块文档。
    pub fn load(&self) -> Result<Settings, ConfigError> {
        let file = match File::open(&self.path) {
            Ok(f) => f,
            // 只有「确实没有这个文件」才等于「用户还没有配置」。
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Settings::default()),
            Err(source) => {
                return Err(ConfigError::io("打开设置文件", &self.path, source));
            }
        };

        let data = read_capped(file, MAX_SETTINGS_BYTES)
            .map_err(|source| ConfigError::io("读取设置文件", &self.path, source))?
            .ok_or_else(|| ConfigError::TooLarge {
                path: self.path.clone(),
                limit: MAX_SETTINGS_BYTES,
            })?;

        let mut settings: Settings =
            serde_json::from_slice(&data).map_err(|source| ConfigError::Malformed {
                path: self.path.clone(),
                source,
            })?;
        settings.normalize();
        Ok(settings)
    }

    /// 原子地写入设置。
    ///
    /// 收 `&Settings` 而不是 `&mut Settings`：规范化只发生在内部的副本上，
    /// 调用方手里那份不会被动。见 [`Settings::normalize`] 里的说明。
    pub fn save(&self, settings: &Settings) -> Result<(), ConfigError> {
        let mut normalized = settings.clone();
        normalized.normalize();

        let data = serde_json::to_vec_pretty(&normalized)
            .map_err(|source| ConfigError::Encode { source })?;

        write_atomically(&self.path, &data)
    }

    /// 把当前这份读不出来的设置文件改名留档，返回留档路径；
    /// 本来就没有文件时返回 `Ok(None)`。
    ///
    /// [`load`](Self::load) 失败之后调用方会放弃写盘，但光是不写还不够：用户
    /// 之后可能想把里面的监控列表捞回来，或者需要这份文件来判断到底哪里坏了。
    ///
    /// 用改名而不是复制，是因为读失败的情形（权限不足、IO 错误）下未必读得动，
    /// 而改名只需要目录的写权限。
    pub fn preserve_corrupted(&self) -> Result<Option<PathBuf>, ConfigError> {
        // 用 symlink_metadata 而不是 metadata：指向不存在目标的符号链接同样是
        // 「这里有个文件挡着」，也同样需要挪开，不该被当成「本来就没有文件」。
        match fs::symlink_metadata(&self.path) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(ConfigError::io("检查设置文件", &self.path, source)),
        }

        let backup = self.backup_path();
        fs::rename(&self.path, &backup)
            .map_err(|source| ConfigError::io("留档损坏的设置文件", &self.path, source))?;
        Ok(Some(backup))
    }

    /// 挑一个还没被占用的留档路径。
    ///
    /// 只挑名字、不预先创建，随后的 rename 才是真正的落地动作。同一秒内连续
    /// 留档两次也不会互相覆盖 —— 覆盖掉的正是用户最想留住的那份。
    fn backup_path(&self) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let name = self
            .path
            .file_name()
            .map_or_else(|| SETTINGS_FILE.to_string(), |n| n.to_string_lossy().into());
        let dir = parent_dir(&self.path);

        let mut candidate = dir.join(format!("{name}.corrupt-{stamp}"));
        let mut seq = 1u32;
        while candidate.symlink_metadata().is_ok() {
            candidate = dir.join(format!("{name}.corrupt-{stamp}-{seq}"));
            seq = seq.saturating_add(1);
        }
        candidate
    }

    /// 尝试读取 Go 版留下的设置并转换过来，供首次启动时平滑迁移。
    ///
    /// 调用方应当只在 [`load`](Self::load) 表明「新版文件还不存在」时才调用它，
    /// 否则会用旧配置盖掉新配置。
    ///
    /// 返回 `Option` 而不是 `Result` 是刻意的：迁移是锦上添花。旧文件不在、
    /// 读不动、格式对不上……任何一种情况都只意味着「没有可迁移的东西」，
    /// 绝不能因此让程序起不来，也不值得为它弹一个用户看不懂的错误。
    pub fn import_legacy(&self) -> Option<Settings> {
        let current_legacy = parent_dir(&self.path).join(LEGACY_SETTINGS_FILE);
        let previous_legacy = self
            .previous_path
            .as_ref()
            .map(|path| parent_dir(path).join(LEGACY_SETTINGS_FILE));

        std::iter::once(current_legacy)
            .chain(previous_legacy)
            .filter(|path| path != &self.path)
            .find_map(|path| {
                let file = File::open(path).ok()?;
                // 旧文件同样要限量读：它会撑爆内存的理由与新文件完全相同，
                // 没道理在迁移路径上开口子。
                let data = read_capped(file, MAX_SETTINGS_BYTES).ok()??;
                let legacy: LegacySettings = serde_json::from_slice(&data).ok()?;
                Some(legacy.into_settings())
            })
    }
}

/// 取文件所在目录；路径本身就是个裸文件名时退回当前目录。
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// 至多读 `limit + 1` 字节。读满 `limit + 1` 说明内容确实超限，返回 `Ok(None)`。
///
/// 多读一个字节是为了在**不整读**的前提下判定超限：只读 `limit` 字节的话，
/// 恰好读满时分不清「文件正好这么大」还是「后面还有几个 GB」。
///
/// 关键在于内存占用被这个上限钉死了，与文件实际大小无关 —— 这正是 Go 版
/// `os.ReadFile` 缺的那一环。
fn read_capped<R: Read>(reader: R, limit: usize) -> std::io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    let mut capped = reader.take(limit as u64 + 1);
    capped.read_to_end(&mut buf)?;
    if buf.len() > limit {
        return Ok(None);
    }
    Ok(Some(buf))
}

/// 临时文件名的进程内序号，保证同一进程并发写盘时不会撞名。
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 临时文件守卫：只要没被 [`TempFile::disarm`]，离开作用域就把它删掉。
///
/// 有了它，写入过程中任何一步出错都不会在配置目录里留下垃圾，而不必在每条
/// 失败分支上手写一遍清理 —— 手写的那种迟早会漏一条。
struct TempFile {
    path: PathBuf,
    armed: bool,
}

impl TempFile {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            // 清理失败也没什么可做的，更不该在析构里制造新的失败。
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// 在 `dir` 下独占地创建一个临时文件。
///
/// 用 `create_new`：如果重名的文件已经存在（另一个进程正在写、或上一次崩溃留下的
/// 残骸），必须换个名字重来，绝不能截断别人的文件。
fn create_temp_file(dir: &Path) -> Result<(TempFile, File), ConfigError> {
    let mut last: Option<std::io::Error> = None;
    for _ in 0..32 {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let candidate = dir.join(format!(".apw-settings-{}-{seq}.tmp", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                return Ok((
                    TempFile {
                        path: candidate,
                        armed: true,
                    },
                    file,
                ));
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(ConfigError::io("创建临时文件于", dir, e)),
        }
    }
    let source =
        last.unwrap_or_else(|| std::io::Error::new(ErrorKind::AlreadyExists, "临时文件名反复冲突"));
    Err(ConfigError::io("创建临时文件于", dir, source))
}

/// 先写临时文件、刷盘、再改名。
///
/// 直接往目标文件上写的话，写到一半崩溃或断电就会留下半截 JSON，下次启动直接
/// 解析失败 —— 用户的监控列表就这么没了。改名在同一个文件系统内是原子的：
/// 任何时刻去读，看到的要么是完整的旧内容，要么是完整的新内容。
fn write_atomically(path: &Path, data: &[u8]) -> Result<(), ConfigError> {
    let dir = parent_dir(path);
    fs::create_dir_all(dir).map_err(|e| ConfigError::io("创建配置目录", dir, e))?;

    let (mut guard, mut file) = create_temp_file(dir)?;

    file.write_all(data)
        .map_err(|e| ConfigError::io("写入临时文件", &guard.path, e))?;
    // 必须在改名前刷盘。只改名不刷盘的话，掉电后可能出现「名字换好了、内容还在
    // 页缓存里没落地」，结果是一个大小为 0 的设置文件 —— 比半截 JSON 还糟。
    file.sync_all()
        .map_err(|e| ConfigError::io("刷盘临时文件", &guard.path, e))?;
    drop(file);

    fs::rename(&guard.path, path).map_err(|e| ConfigError::io("替换设置文件", path, e))?;
    // 改名成功，临时文件已经不在原路径上了，撤掉守卫免得误删刚写好的设置。
    guard.disarm();

    // 顺带把目录项也刷一次，让这次改名在掉电后仍然可见。这一步是尽力而为：
    // 它失败不代表数据有问题（新内容已经完整落盘、改名对其他读者也已生效），
    // 为此让 save 整个报错反而会误导用户去做无谓的补救。
    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
    Ok(())
}

// ---- Go 版设置的读取结构。
// ---- 单独写一套而不是给 Settings 加别名：两版字段命名规则整体不同
// ---- （snake_case vs camelCase），用 alias 打补丁会让新版的线上格式变得含糊，
// ---- 而那个格式是前端 TypeScript 类型照着写的，必须保持唯一。

#[derive(Debug, Deserialize)]
struct LegacySettings {
    #[serde(default)]
    locale: Option<String>,
    #[serde(default)]
    targets: Vec<LegacyTarget>,
    /// Go 版这个字段是有符号 `int`，磁盘上完全可能是 0 甚至负数。
    /// 直接按 `u64` 接会让整份迁移因为一个字段而失败，所以先用 `i64` 接住，
    /// 越界的值交给 [`Settings::normalize`] 收敛。
    #[serde(default)]
    interval_seconds: Option<i64>,
    #[serde(default)]
    bark_url: Option<String>,
    #[serde(default)]
    sound_enabled: Option<bool>,
    #[serde(default)]
    open_bag_on_hit: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct LegacyTarget {
    #[serde(default)]
    locale: String,
    #[serde(default)]
    store_number: String,
    #[serde(default)]
    store_title: String,
    #[serde(default)]
    part_number: String,
    #[serde(default)]
    product_name: String,
}

impl LegacyTarget {
    fn into_target(self) -> Target {
        Target {
            locale: self.locale,
            store_number: self.store_number,
            store_title: self.store_title,
            part_number: self.part_number,
            product_name: self.product_name,
        }
    }
}

impl LegacySettings {
    fn into_settings(self) -> Settings {
        let fallback = Settings::default();
        // 缺失的字段取新版默认值，而不是取零值：Go 版的 SoundEnabled 默认为 true，
        // 一个缺字段的老文件不该在迁移途中把用户的提示音悄悄关掉。
        let mut settings = Settings {
            locale: self.locale.unwrap_or(fallback.locale),
            targets: self
                .targets
                .into_iter()
                .map(LegacyTarget::into_target)
                .collect(),
            interval_seconds: self
                .interval_seconds
                .and_then(|v| u64::try_from(v).ok())
                .unwrap_or(fallback.interval_seconds),
            bark_url: self.bark_url.unwrap_or(fallback.bark_url),
            product_bark_urls: BTreeMap::new(),
            sound_enabled: self.sound_enabled.unwrap_or(fallback.sound_enabled),
            open_on_hit: match self.open_bag_on_hit {
                Some(true) => OpenOnHit::Bag,
                Some(false) => OpenOnHit::None,
                None => fallback.open_on_hit,
            },
        };
        settings.normalize();
        settings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn 目标(store: &str, part: &str) -> Target {
        Target {
            locale: "zh_CN".into(),
            store_number: store.into(),
            store_title: "上海-环球港".into(),
            part_number: part.into(),
            product_name: "iPhone 17 512GB 黑色".into(),
        }
    }

    #[test]
    fn 默认设置本身就是规范化的() {
        let mut s = Settings::default();
        let before = s.clone();
        s.normalize();
        // 默认值再规范化一次必须原地不动，否则「默认」和「合法」就是两回事，
        // 存取往返会莫名其妙地改内容。
        assert_eq!(before, s);
        assert_eq!(s.interval_seconds, DEFAULT_INTERVAL_SECONDS);
        assert!(s.sound_enabled);
        assert_eq!(s.open_on_hit, OpenOnHit::Bag);
        assert!(region_by_locale(&s.locale).is_some());
    }

    #[test]
    fn 规范化修正非法地区() {
        let mut s = Settings {
            locale: "de_DE".into(),
            ..Settings::default()
        };
        s.normalize();
        assert_eq!(s.locale, fallback_locale());

        // 合法地区不能被动。
        let mut ok = Settings {
            locale: "ja_JP".into(),
            ..Settings::default()
        };
        ok.normalize();
        assert_eq!(ok.locale, "ja_JP");
    }

    #[test]
    fn 规范化把过小的间隔退回默认值() {
        for bad in [0, 1, MIN_INTERVAL_SECONDS - 1] {
            let mut s = Settings {
                interval_seconds: bad,
                ..Settings::default()
            };
            s.normalize();
            assert_eq!(
                s.interval_seconds, DEFAULT_INTERVAL_SECONDS,
                "间隔 {bad} 秒应当退回默认值"
            );
        }

        // 下限本身是允许的，用户有意调快的余地要留着。
        let mut s = Settings {
            interval_seconds: MIN_INTERVAL_SECONDS,
            ..Settings::default()
        };
        s.normalize();
        assert_eq!(s.interval_seconds, MIN_INTERVAL_SECONDS);
        assert_eq!(s.interval(), Duration::from_secs(MIN_INTERVAL_SECONDS));
    }

    #[test]
    fn 规范化去重并保留首次出现的顺序() {
        let mut s = Settings {
            targets: vec![
                目标("R683", "MG724CH/A"),
                目标("R448", "MG0A4CH/A"),
                目标("R683", "MG724CH/A"),
            ],
            ..Settings::default()
        };
        s.normalize();
        assert_eq!(s.targets.len(), 2);
        assert_eq!(s.targets[0].store_number, "R683");
        assert_eq!(s.targets[1].store_number, "R448");
    }

    #[test]
    fn 规范化丢掉发不出请求的目标() {
        let mut s = Settings {
            targets: vec![
                目标("", "MG724CH/A"),
                目标("R683", ""),
                目标("R683", "   "),
                Target {
                    locale: String::new(),
                    ..目标("R448", "MG0A4CH/A")
                },
                目标("R683", "MG724CH/A"),
            ],
            ..Settings::default()
        };
        s.normalize();
        // 只剩那条字段齐全的。地区/门店/零件号任缺其一都发不出请求，留着只会在
        // 界面上永远显示「未知」，让用户以为是程序坏了。
        assert_eq!(s.targets, vec![目标("R683", "MG724CH/A")]);
    }

    #[test]
    fn 规范化是幂等的() {
        let mut once = Settings {
            locale: "de_DE".into(),
            interval_seconds: 1,
            targets: vec![目标("R683", "MG724CH/A"), 目标("R683", "MG724CH/A")],
            ..Settings::default()
        };
        once.normalize();
        let mut twice = once.clone();
        twice.normalize();
        assert_eq!(once, twice);
    }

    #[test]
    fn 限量读取不会把整个文件吃进内存() {
        // std::io::repeat 是一个无限长的读取源。如果实现是「先整读再判断大小」，
        // 这个用例会一直分配内存直到进程被杀 —— 它跑得完，本身就是证明。
        // 这正是 Go 版 os.ReadFile 缺的那一环。
        let got = read_capped(std::io::repeat(b'x'), MAX_SETTINGS_BYTES).expect("读取不该失败");
        assert!(got.is_none(), "无限长的内容必须被判为超限");
    }

    #[test]
    fn 限量读取的边界正好落在上限上() {
        let exact = vec![b'x'; MAX_SETTINGS_BYTES];
        let got = read_capped(exact.as_slice(), MAX_SETTINGS_BYTES).expect("读取不该失败");
        assert_eq!(got.map(|v| v.len()), Some(MAX_SETTINGS_BYTES));

        let over = vec![b'x'; MAX_SETTINGS_BYTES + 1];
        let got = read_capped(over.as_slice(), MAX_SETTINGS_BYTES).expect("读取不该失败");
        assert!(got.is_none());
    }

    #[test]
    fn 裸文件名的父目录是当前目录() {
        assert_eq!(parent_dir(Path::new("settings.v2.json")), Path::new("."));
        assert_eq!(
            parent_dir(Path::new("/a/b/settings.v2.json")),
            Path::new("/a/b")
        );
    }

    #[test]
    fn 旧版负数间隔会被收敛掉而不是让整份迁移失败() {
        let legacy: LegacySettings =
            serde_json::from_str(r#"{"locale":"zh_CN","interval_seconds":-1}"#)
                .expect("旧版的有符号间隔必须能解析");
        let s = legacy.into_settings();
        assert_eq!(s.interval_seconds, DEFAULT_INTERVAL_SECONDS);
    }

    #[test]
    fn 旧版缺失的开关取默认值而不是取假() {
        let legacy: LegacySettings =
            serde_json::from_str(r#"{"locale":"zh_CN"}"#).expect("缺字段的旧文件也要能解析");
        let s = legacy.into_settings();
        // Go 版 Default() 里这两项都是 true。缺字段当成 false 会让用户在完全
        // 没动过设置的情况下，升级之后提示音自己关掉了。
        assert!(s.sound_enabled);
        assert_eq!(s.open_on_hit, OpenOnHit::Bag);
    }

    #[test]
    fn 定位配置目录时不碰磁盘() {
        // new() 只算路径。构造一个 Store 就在用户机器上建目录，会让「装了但从没
        // 用过」也留下痕迹；目录该在真正写盘时才出现。
        let Ok(store) = SettingsStore::new() else {
            return; // 没有配置目录的环境跳过，这不是本用例要验证的事。
        };
        assert!(
            store
                .path()
                .ends_with(Path::new(APP_DIR).join(SETTINGS_FILE))
        );
        // 新版必须和 Go 版分家，否则两个版本会互相覆盖对方的配置。
        assert_ne!(SETTINGS_FILE, LEGACY_SETTINGS_FILE);
    }
}
