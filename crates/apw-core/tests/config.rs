//! 设置读写的落盘行为测试。
//!
//! 这些用例都要碰真实文件系统 —— 原子写入、大小上限、损坏留档这几件事，
//! 用假的存储层测等于什么都没测。
//!
//! 大部分用例对应 Go 版被独立审查挑出来的一条真实缺陷。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use apw_core::config::{
    ConfigError, DEFAULT_INTERVAL_SECONDS, MAX_SETTINGS_BYTES, Settings, SettingsStore,
};
use apw_core::model::Target;

/// 用完自动删掉的临时目录。
///
/// 项目没有引入 tempfile 依赖，这里自己搭一个够用的：名字里带进程号、纳秒和
/// 自增序号，同一台机器上并行跑测试也不会撞。
struct 临时目录 {
    path: PathBuf,
}

impl 临时目录 {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "apw-config-{tag}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("创建临时目录失败");
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// 新版设置文件应该在的位置。
    fn 设置路径(&self) -> PathBuf {
        self.join("settings.v2.json")
    }

    fn 条目名(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.path)
            .expect("列目录失败")
            .map(|e| {
                e.expect("读目录项失败")
                    .file_name()
                    .to_string_lossy()
                    .into()
            })
            .collect();
        names.sort();
        names
    }
}

impl Drop for 临时目录 {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn 目标(store: &str, part: &str) -> Target {
    Target {
        locale: "zh_CN".into(),
        store_number: store.into(),
        store_title: "上海-环球港".into(),
        part_number: part.into(),
        product_name: "iPhone 17 512GB 黑色".into(),
    }
}

fn 样例设置() -> Settings {
    Settings {
        locale: "zh_CN".into(),
        targets: vec![目标("R683", "MG724CH/A"), 目标("R448", "MG0A4CH/A")],
        interval_seconds: 45,
        bark_url: "https://api.day.app/xxxx".into(),
        sound_enabled: false,
        open_bag_on_hit: true,
    }
}

#[test]
fn 存取往返内容不变() {
    let dir = 临时目录::new("roundtrip");
    let store = SettingsStore::at(dir.设置路径());

    let want = 样例设置();
    store.save(&want).expect("保存失败");
    let got = store.load().expect("读取失败");
    assert_eq!(got, want);

    // 再存再取仍然一致：规范化不能每过一轮就悄悄改一点内容。
    store.save(&got).expect("再次保存失败");
    assert_eq!(store.load().expect("再次读取失败"), want);
}

#[test]
fn 保存会顺手创建不存在的配置目录() {
    let dir = 临时目录::new("mkdir");
    // 指到一层还不存在的子目录里 —— new() 刻意不建目录，写盘时才建。
    let store = SettingsStore::at(dir.join("nested").join("settings.v2.json"));
    store.save(&样例设置()).expect("保存失败");
    assert_eq!(store.load().expect("读取失败"), 样例设置());
}

#[test]
fn 文件不存在时返回默认设置且不算错误() {
    let dir = 临时目录::new("missing");
    let store = SettingsStore::at(dir.设置路径());

    let got = store.load().expect("文件不存在不该报错");
    assert_eq!(got, Settings::default());
    // 读一次不该把文件建出来：光是启动程序不等于用户已经有配置了。
    assert!(dir.条目名().is_empty());
}

#[test]
fn 文件损坏时返回错误而不是默认设置() {
    let dir = 临时目录::new("corrupt");
    let path = dir.设置路径();
    fs::write(&path, b"{ this is not json").expect("写测试文件失败");

    let store = SettingsStore::at(path.clone());
    // 这一条是整个模块的要害：读不到旧配置**不等于**用户没有配置。
    // 若这里返回默认值，调用方会照常继续并在下一次改动时写盘，几毫秒内就用
    // 一份空配置盖掉用户攒了很久的监控列表，全程没有任何提示。
    let err = store
        .load()
        .expect_err("坏文件绝不能被当成「用户没有配置」而返回默认值");
    assert!(
        matches!(err, ConfigError::Malformed { .. }),
        "实际是 {err:?}"
    );

    // 报错归报错，原文件必须原封不动地留在那，等着 preserve_corrupted 去留档。
    assert_eq!(
        fs::read(&path).expect("原文件应当还在"),
        b"{ this is not json".to_vec()
    );
}

#[test]
fn 空文件也算损坏() {
    let dir = 临时目录::new("empty");
    let path = dir.设置路径();
    fs::write(&path, b"").expect("写测试文件失败");

    let err = SettingsStore::at(path).load().expect_err("空文件必须报错");
    // 零字节最可能是上一次写盘写到一半崩了。它和「文件不存在」看着像，处置
    // 却相反：一个要留档保命，一个可以放心用默认值。
    assert!(
        matches!(err, ConfigError::Malformed { .. }),
        "实际是 {err:?}"
    );
}

#[test]
fn 超大文件返回可区分的错误() {
    let dir = 临时目录::new("toolarge");
    let path = dir.设置路径();

    {
        let mut f = fs::File::create(&path).expect("创建测试文件失败");
        // 前面是一份合法 JSON，后面缀上超过上限的填充：证明拦截发生在大小检查这一步，
        // 而不是碰巧因为解析失败。
        f.write_all(br#"{"locale":"zh_CN"}"#).expect("写入失败");
        let block = vec![b' '; 64 * 1024];
        let mut written = 0usize;
        while written <= MAX_SETTINGS_BYTES {
            f.write_all(&block).expect("写入失败");
            written += block.len();
        }
    }
    assert!(fs::metadata(&path).expect("取文件大小失败").len() as usize > MAX_SETTINGS_BYTES);

    let err = SettingsStore::at(path)
        .load()
        .expect_err("超大文件必须报错");
    // 用 matches! 就能和「JSON 坏了」分开 —— 两者给用户的提示完全不同。
    assert!(
        matches!(err, ConfigError::TooLarge { limit, .. } if limit == MAX_SETTINGS_BYTES),
        "实际是 {err:?}"
    );
}

#[test]
fn 恰好卡在上限的文件仍然能读出来() {
    let dir = 临时目录::new("exact");
    let path = dir.设置路径();

    // 合法 JSON 后面补空白凑到正好 MAX_SETTINGS_BYTES 字节。上限是防御用的，
    // 不该把边界上那份合法配置也一并拒掉。
    let head = br#"{"locale":"ja_JP","intervalSeconds":60}"#;
    let mut data = head.to_vec();
    data.resize(MAX_SETTINGS_BYTES, b' ');
    fs::write(&path, &data).expect("写测试文件失败");

    let got = SettingsStore::at(path)
        .load()
        .expect("恰好卡在上限的文件应当能读");
    assert_eq!(got.locale, "ja_JP");
    assert_eq!(got.interval_seconds, 60);
}

#[test]
fn 读出来的设置会被规范化() {
    let dir = 临时目录::new("normalize");
    let path = dir.设置路径();
    // 手工编辑或旧版本留下的非法内容：地区认不出、间隔小到必被风控、目标重复。
    fs::write(
        &path,
        r#"{
          "locale": "de_DE",
          "intervalSeconds": 1,
          "targets": [
            {"locale":"zh_CN","storeNumber":"R683","storeTitle":"上海-环球港","partNumber":"MG724CH/A","productName":"x"},
            {"locale":"zh_CN","storeNumber":"R683","storeTitle":"上海-环球港","partNumber":"MG724CH/A","productName":"x"}
          ]
        }"#,
    )
    .expect("写测试文件失败");

    let got = SettingsStore::at(path).load().expect("读取失败");
    assert_ne!(got.locale, "de_DE");
    assert_eq!(got.interval_seconds, DEFAULT_INTERVAL_SECONDS);
    assert_eq!(got.targets.len(), 1);
}

#[test]
fn 缺字段的文件取默认值而不是取零值() {
    let dir = 临时目录::new("partial");
    let path = dir.设置路径();
    fs::write(&path, br#"{"locale":"zh_CN"}"#).expect("写测试文件失败");

    let got = SettingsStore::at(path).load().expect("读取失败");
    // 老版本写下的文件可能少几个字段。缺字段当零值处理的话，用户什么都没改，
    // 提示音和自动开购物袋却会自己关掉。
    assert!(got.sound_enabled);
    assert!(got.open_bag_on_hit);
    assert_eq!(got.interval_seconds, DEFAULT_INTERVAL_SECONDS);
}

#[test]
fn 保存不改动调用方持有的数据() {
    let dir = 临时目录::new("noalias");
    let store = SettingsStore::at(dir.设置路径());

    let mine = Settings {
        locale: "de_DE".into(),
        interval_seconds: 1,
        targets: vec![
            目标("R683", "MG724CH/A"),
            目标("R683", "MG724CH/A"),
            目标("R448", "MG0A4CH/A"),
        ],
        ..Settings::default()
    };
    let 原样 = mine.clone();

    store.save(&mine).expect("保存失败");

    // Go 版 Normalize 用 s.Targets[:0] 就地压缩，会把调用方共用的底层数组从
    // [A, A, B] 改成 [A, B, B]（长度仍是 3），于是「改设置 → 保存 →
    // 把 targets 交给引擎」这条路径会莫名多出一条重复的监控行。
    // 这里 save 收的是 &Settings，规范化只发生在内部副本上。
    assert_eq!(mine, 原样, "save 不得改动调用方手里的数据");
    assert_eq!(mine.targets.len(), 3);

    // 落到盘上的那份则必须是规范化过的。
    let 盘上 = store.load().expect("读取失败");
    assert_eq!(盘上.targets.len(), 2);
    assert_eq!(盘上.interval_seconds, DEFAULT_INTERVAL_SECONDS);
    assert_ne!(盘上.locale, "de_DE");
}

#[test]
fn 原子写入后目录里没有残留临时文件() {
    let dir = 临时目录::new("atomic");
    let store = SettingsStore::at(dir.设置路径());

    for _ in 0..5 {
        store.save(&样例设置()).expect("保存失败");
    }

    // 先写临时文件再改名是为了避免半截 JSON；但每轮都留个 .tmp 下来同样是缺陷，
    // 配置目录几个月后会堆满垃圾，用户还分不清哪个才是真的设置。
    assert_eq!(dir.条目名(), vec!["settings.v2.json".to_string()]);
}

#[test]
fn 保存不会写出半截文件() {
    let dir = 临时目录::new("halfwrite");
    let path = dir.设置路径();
    let store = SettingsStore::at(path.clone());

    store.save(&样例设置()).expect("首次保存失败");
    let 第一份 = fs::read(&path).expect("读原文件失败");

    // 换一份明显更短的内容再存一次：如果实现是「截断原文件再往上写」，
    // 中途崩溃就会留下前一份的尾巴。改名的做法下，任何时刻读到的都是完整的一份。
    store.save(&Settings::default()).expect("第二次保存失败");
    let 第二份 = fs::read(&path).expect("读新文件失败");

    assert_ne!(第一份, 第二份);
    let _: Settings = serde_json::from_slice(&第二份).expect("盘上的内容必须是完整合法的 JSON");
    assert_eq!(store.load().expect("读取失败"), Settings::default());
}

#[test]
fn 留档把坏文件改名保存下来() {
    let dir = 临时目录::new("preserve");
    let path = dir.设置路径();
    fs::write(&path, b"{ broken").expect("写测试文件失败");

    let store = SettingsStore::at(path.clone());
    let backup = store
        .preserve_corrupted()
        .expect("留档不该失败")
        .expect("有文件时必须返回留档路径");

    // 原路径腾空，坏内容一个字节不少地挪到了留档路径上 —— 用户之后还能把
    // 里面的监控列表捞回来，也能拿它去查到底是哪里坏了。
    assert!(!path.exists());
    assert_eq!(fs::read(&backup).expect("读留档失败"), b"{ broken".to_vec());
    assert!(
        backup
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains("corrupt")),
        "留档文件名要一眼看得出是什么：{backup:?}"
    );

    // 腾空之后再读就是「用户没有配置」，可以安全地用默认值继续。
    assert_eq!(store.load().expect("读取失败"), Settings::default());
}

#[test]
fn 连续留档不会互相覆盖() {
    let dir = 临时目录::new("preserve-twice");
    let path = dir.设置路径();
    let store = SettingsStore::at(path.clone());

    fs::write(&path, b"first").expect("写测试文件失败");
    let 第一次 = store
        .preserve_corrupted()
        .expect("留档失败")
        .expect("应有留档");
    fs::write(&path, b"second").expect("写测试文件失败");
    let 第二次 = store
        .preserve_corrupted()
        .expect("留档失败")
        .expect("应有留档");

    // 同一秒内连留两次时若用同一个名字，第二次覆盖掉的正是用户最想留住的那份。
    assert_ne!(第一次, 第二次);
    assert_eq!(fs::read(&第一次).expect("读留档失败"), b"first".to_vec());
    assert_eq!(fs::read(&第二次).expect("读留档失败"), b"second".to_vec());
}

#[test]
fn 没有文件时留档返回空且不报错() {
    let dir = 临时目录::new("preserve-none");
    let store = SettingsStore::at(dir.设置路径());
    assert_eq!(store.preserve_corrupted().expect("不该报错"), None);
    assert!(dir.条目名().is_empty());
}

#[test]
fn 迁移能读出旧版格式() {
    let dir = 临时目录::new("legacy");
    // Go 版的文件名和字段命名都和新版不同：settings.json + snake_case。
    fs::write(
        dir.join("settings.json"),
        r#"{
          "locale": "zh_HK",
          "targets": [
            {"locale":"zh_HK","store_number":"R409","store_title":"香港-銅鑼灣","part_number":"MG724ZA/A","product_name":"iPhone 17 512GB 黑色"}
          ],
          "interval_seconds": 60,
          "bark_url": "https://api.day.app/legacy",
          "sound_enabled": false,
          "open_bag_on_hit": true
        }"#,
    )
    .expect("写旧版文件失败");

    let store = SettingsStore::at(dir.设置路径());
    let got = store.import_legacy().expect("旧版设置应当能读出来");

    assert_eq!(got.locale, "zh_HK");
    assert_eq!(got.interval_seconds, 60);
    assert_eq!(got.bark_url, "https://api.day.app/legacy");
    assert!(!got.sound_enabled);
    assert!(got.open_bag_on_hit);
    assert_eq!(got.targets.len(), 1);
    assert_eq!(got.targets[0].store_number, "R409");
    assert_eq!(got.targets[0].store_title, "香港-銅鑼灣");
    assert_eq!(got.targets[0].part_number, "MG724ZA/A");

    // 迁移是只读的：旧文件必须原封不动地留着，用户想退回 Go 版还得靠它。
    assert!(dir.join("settings.json").exists());
    // 迁移本身也不写盘，落盘与否由调用方决定。
    assert!(!dir.设置路径().exists());
}

#[test]
fn 迁移读不到时返回空() {
    let dir = 临时目录::new("legacy-missing");
    let store = SettingsStore::at(dir.设置路径());

    // 旧文件根本不在。
    assert_eq!(store.import_legacy(), None);

    // 旧文件在但不是 JSON。迁移是锦上添花，绝不能因为它让程序起不来。
    fs::write(dir.join("settings.json"), b"not json at all").expect("写测试文件失败");
    assert_eq!(store.import_legacy(), None);

    // 旧文件大得离谱：这条路径同样要限量读，没道理在迁移上开个口子。
    // 内容本身是合法 JSON，只是后面缀了超限的填充 —— 被拦下来只能是因为大小。
    let mut 巨大 = br#"{"locale":"zh_CN"}"#.to_vec();
    巨大.resize(MAX_SETTINGS_BYTES + 1, b' ');
    fs::write(dir.join("settings.json"), &巨大).expect("写测试文件失败");
    assert_eq!(store.import_legacy(), None);
}

#[test]
fn 迁移过来的非法字段会被规范化() {
    let dir = 临时目录::new("legacy-normalize");
    fs::write(
        dir.join("settings.json"),
        br#"{"locale":"de_DE","interval_seconds":-1,
             "targets":[
               {"locale":"zh_CN","store_number":"R683","part_number":"MG724CH/A"},
               {"locale":"zh_CN","store_number":"R683","part_number":"MG724CH/A"}
             ]}"#,
    )
    .expect("写测试文件失败");

    let got = SettingsStore::at(dir.设置路径())
        .import_legacy()
        .expect("应当能读出来");
    // Go 版的 interval_seconds 是有符号 int，磁盘上出现负数完全可能。按 u64 硬接
    // 会让整份迁移因为一个字段失败，用户的监控列表就白白丢了。
    assert_eq!(got.interval_seconds, DEFAULT_INTERVAL_SECONDS);
    assert_ne!(got.locale, "de_DE");
    assert_eq!(got.targets.len(), 1);
}

#[test]
fn 新旧两版的设置文件互不干扰() {
    let dir = 临时目录::new("coexist");
    let store = SettingsStore::at(dir.设置路径());

    let 旧内容 =
        br#"{"locale":"ja_JP","interval_seconds":60,"sound_enabled":false,"open_bag_on_hit":false}"#
            .to_vec();
    fs::write(dir.join("settings.json"), &旧内容).expect("写旧版文件失败");

    // 新版写盘只动 settings.v2.json。共用一个文件名的话，两个版本会把对方的
    // 配置读成一堆缺省值再覆盖掉 —— 用户想退回 Go 版时才发现设置全没了。
    store.save(&样例设置()).expect("保存失败");
    assert_eq!(
        fs::read(dir.join("settings.json")).expect("读旧版文件失败"),
        旧内容
    );
    assert_eq!(store.load().expect("读取失败"), 样例设置());

    assert_eq!(
        dir.条目名(),
        vec!["settings.json".to_string(), "settings.v2.json".to_string()]
    );
}

#[test]
fn 设置的线上格式是小驼峰() {
    // 前端的 TypeScript 类型是照着这个形状手写的，字段名一改界面就整片读不到值。
    let value = serde_json::to_value(样例设置()).expect("序列化失败");
    let obj = value.as_object().expect("应当是个对象");

    for key in [
        "locale",
        "targets",
        "intervalSeconds",
        "barkUrl",
        "soundEnabled",
        "openBagOnHit",
    ] {
        assert!(obj.contains_key(key), "缺少字段 {key}：{value}");
    }
    assert!(!obj.contains_key("interval_seconds"), "不该有蛇形字段");
    assert_eq!(obj.len(), 6);
}

#[test]
fn 配置文件路径落在用户配置目录下() {
    let Ok(store) = SettingsStore::new() else {
        return; // 环境里没有配置目录，跳过。
    };
    // 上游把 user_settings.json 写在进程工作目录，打包成 macOS .app 之后工作目录
    // 取决于应用被如何启动，可能是 / 这种不可写的位置，设置会静默丢失。
    assert!(store.path().is_absolute(), "配置路径必须是绝对路径");
    assert!(
        store
            .path()
            .ends_with(Path::new("apple-store-inventory-monitor").join("settings.v2.json"))
    );
}
