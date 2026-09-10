#!/usr/bin/env python3
"""重新生成 products_<locale>.json 离线快照。

这些快照是断网或被拦截时的兜底目录，正常情况下程序会从购买页现抓（见
crates/apw-core/src/apple_catalog.rs）。上游是把 productSelectionData 手工从
浏览器开发者工具里复制进仓库，于是每次 Apple 发新机都得等作者更新并发版；
这个脚本把那一步变成可重复的操作。

    python3 crates/apw-core/data/generate.py              # 重新抓取并写入
    python3 crates/apw-core/data/generate.py --self-test  # 不联网，自检解析口径

输出的每个元素是「一页购买页」：

    {"category": "mac", "family": "macbook-air", "data": {products, displayValues}}

data 的结构与购买页里那个 productSelectionData 对象一致，Rust 侧两边共用同一套
解析。这里只把用得到的字段挑出来 —— 原始对象里塞满了图片、价格、埋点，
整页留着会让二进制大出一个数量级，而那些字段一个也用不上。
"""

from html.parser import HTMLParser
import gzip
import json
import re
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

UA = (
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36"
)

# 与 crates/apw-core/src/model.rs 的 REGIONS 保持一致。
REGIONS = {
    "zh_CN": ("https://www.apple.com.cn", "zh-CN,zh;q=0.9"),
    "zh_HK": ("https://www.apple.com/hk-zh", "zh-HK,zh;q=0.9"),
    "zh_TW": ("https://www.apple.com/tw", "zh-TW,zh;q=0.9"),
    "ja_JP": ("https://www.apple.com/jp", "ja-JP,ja;q=0.9"),
    "en_SG": ("https://www.apple.com/sg", "en-US,en;q=0.9"),
    "en_AU": ("https://www.apple.com/au", "en-US,en;q=0.9"),
    "en_MY": ("https://www.apple.com/my", "en-US,en;q=0.9"),
}

# 与 crates/apw-core/src/model.rs 的 DEFAULT_FAMILIES 保持一致。
FAMILIES = [
    ("iphone", "iphone-17"),
    ("iphone", "iphone-18-pro"),
    ("iphone", "iphone-duo"),
    ("iphone", "iphone-17e"),
    ("iphone", "iphone-16"),
    ("iphone", "iphone-air"),
    ("ipad", "ipad-pro"),
    ("ipad", "ipad-air"),
    ("ipad", "ipad"),
    ("ipad", "ipad-mini"),
    ("mac", "macbook-air"),
    ("mac", "macbook-pro"),
    ("mac", "macbook-neo"),
    ("mac", "imac"),
    ("mac", "mac-mini"),
    ("mac", "mac-studio"),
    ("mac", "studio-display"),
    ("mac", "studio-display-xdr"),
    ("watch", "apple-watch"),
    ("watch", "apple-watch-se"),
    ("watch", "apple-watch-ultra"),
    ("watch", "apple-watch-hermes"),
    ("watch", "apple-watch-hermes-ultra"),
]

BUY_PATH = {"iphone": "buy-iphone", "ipad": "buy-ipad", "mac": "buy-mac", "watch": "buy-watch"}

MARKER = b"PRODUCT_SELECTION_BOOTSTRAP"
KEY = b"productSelectionData"

# 请求之间的最小间隔。抓的是公开页面，但没有理由把自己送进风控。
DELAY_SECONDS = 1.5
RETRIES = 3


def fetch(url: str, accept_language: str) -> bytes:
    request = urllib.request.Request(
        url,
        headers={
            "User-Agent": UA,
            "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            "Accept-Language": accept_language,
            "Accept-Encoding": "gzip",
        },
    )
    with urllib.request.urlopen(request, timeout=40) as response:
        body = response.read()
        if response.headers.get("Content-Encoding") == "gzip":
            body = gzip.decompress(body)
        return body


def is_ident_byte(b: bytes) -> bool:
    return b.isalnum() or b in (b"_", b"$")


def extract(page: bytes):
    """把 productSelectionData 的值截出来。

    与 Rust 侧 `extract_product_selection_data` 同一套做法，三条都要有：

    1. 先定位到 bootstrap 变量，避开页面别处的同名文本；
    2. 键名前后必须是标识符边界。否则 `legacy_productSelectionData` 这种把目标名
       包在里面的更长标识符也会算命中，而它后面恰好也是冒号加对象 —— 于是脚本
       会把一份**早已下架的旧目录**写进快照。生成的文件是合法 JSON、商品也非空，
       Rust 加载和现有测试全都会通过，没有任何迹象说明那是旧数据；
    3. 每处候选都要试，不能只认第一个。
    """
    start = page.find(MARKER)
    search = page[start:] if start >= 0 else page

    frm = 0
    while True:
        at = search.find(KEY, frm)
        if at < 0:
            return None
        frm = at + len(KEY)

        if at > 0 and is_ident_byte(search[at - 1 : at]):
            continue
        if is_ident_byte(search[at + len(KEY) : at + len(KEY) + 1]):
            continue

        pos = at + len(KEY)
        while search[pos : pos + 1] in b" \t\r\n":
            pos += 1
        if search[pos : pos + 1] in (b'"', b"'"):
            pos += 1
            while search[pos : pos + 1] in b" \t\r\n":
                pos += 1
        if search[pos : pos + 1] != b":":
            continue
        pos += 1
        while search[pos : pos + 1] in b" \t\r\n":
            pos += 1
        if search[pos : pos + 1] != b"{":
            continue

        depth, in_string, quote, escaped = 0, False, b"", False
        for i in range(pos, len(search)):
            ch = search[i : i + 1]
            if in_string:
                if escaped:
                    escaped = False
                elif ch == b"\\":
                    escaped = True
                elif ch == quote:
                    in_string = False
                continue
            if ch in (b'"', b"'"):
                in_string, quote = True, ch
            elif ch == b"{":
                depth += 1
            elif ch == b"}":
                depth -= 1
                if depth == 0:
                    try:
                        data = json.loads(search[pos : i + 1])
                        class MacLinks(HTMLParser):
                            def __init__(self):
                                super().__init__()
                                self.urls = set()
                            def handle_starttag(self, tag, attrs):
                                if tag == "a":
                                    for key, value in attrs:
                                        if key == "href" and value and "/shop/buy-mac/" in value:
                                            if len(value.split("/shop/buy-mac/", 1)[1].strip("/").split("/")) > 1:
                                                self.urls.add(value.strip())
                        links = MacLinks()
                        links.feed(page.decode("utf-8", errors="replace"))
                        if links.urls:
                            data["macProductLinks"] = sorted(links.urls)
                        return data
                    except ValueError:
                        break
        continue


def trim(data: dict):
    """只留下 Rust 侧真正会读的字段。"""
    kept_products = []
    used: dict[str, set] = {}

    for raw in data.get("products") or []:
        dimensions = raw.get("dimensions")
        item = {}

        dims = {}
        if isinstance(dimensions, dict):
            dims = {k: v for k, v in dimensions.items() if isinstance(v, str) and v.strip()}

        # 零件号的三个来源，口径必须与 RawProduct::part_number 一模一样，否则
        # 同一台机器会「在线选得到、离线选不到」—— 最难查的那种不一致。
        # 其中 part 要过两道关：这条记录得真的有可用维度（iPad 的平铺记录上
        # 也有 part，装的是产品线代号），且取值得带斜杠（零件号都带，代号不带）。
        for field in ("partNumber", "btrOrFdPartNumber"):
            if raw.get(field):
                item[field] = raw[field]
        if dims and isinstance(raw.get("part"), str) and "/" in raw["part"]:
            item["part"] = raw["part"]
        if not any(item.get(f) for f in ("partNumber", "btrOrFdPartNumber", "part")):
            continue

        if raw.get("familyType"):
            item["familyType"] = raw["familyType"]
        if raw.get("aosContainerPartNumber"):
            item["aosContainerPartNumber"] = raw["aosContainerPartNumber"]

        if isinstance(dimensions, dict):
            if dims:
                item["dimensions"] = dims
            for key, value in dims.items():
                used.setdefault(key, set()).add(value)
        else:
            for key, value in raw.items():
                if key == "dimensionSteporder" or not key.startswith("dimension"):
                    continue
                if isinstance(value, str) and value.strip():
                    item[key] = value
                    used.setdefault(key, set()).add(value)

        kept_products.append(item)

    # 按零件号排一遍。Apple 每次返回的商品顺序都不一样，不排的话每次重新生成
    # 都会产生一份「几百行变更、实际内容一字未改」的假 diff，真正的变化
    # （少了一个型号、多了一个颜色）反而没人看得见。
    kept_products.sort(key=lambda item: item.get("partNumber") or item.get("btrOrFdPartNumber") or item.get("part") or "")

    out = {"products": kept_products}
    if isinstance(data.get("macProductLinks"), list):
        out["macProductLinks"] = data["macProductLinks"]
    # 代数以及 Ultra 的固定规格位于无脚本商品入口，不能在裁剪时丢掉。
    examples = data.get("watchProductSelectionDataNoJS")
    if isinstance(examples, list):
        out["watchProductSelectionDataNoJS"] = [
            {key: entry[key] for key in ("text", "url") if isinstance(entry.get(key), str)}
            for entry in examples if isinstance(entry, dict)
        ]

    # 展示文案只留被引用到的那些取值，而且每条只留一个字段：Apple 在
    # value / header / text 三个字段名之间摇摆，Rust 侧按这个顺序挨个试。
    for source in ("displayValues", "mainDisplayValues"):
        groups = data.get(source)
        if not isinstance(groups, dict):
            continue
        trimmed = {}
        for key, values in groups.items():
            if key not in used or not isinstance(values, dict):
                continue
            entries = {}
            for value in sorted(used[key]):
                entry = values.get(value)
                if not isinstance(entry, dict):
                    continue
                for field in ("value", "header", "text"):
                    if isinstance(entry.get(field), str) and entry[field].strip():
                        entries[value] = {field: entry[field]}
                        break
            if entries:
                trimmed[key] = entries
        if trimmed:
            out[source] = trimmed

    return out


# ---- 自检 ----
#
# 这个脚本产出的东西没法靠 Rust 测试兜住：它要是把一份**旧目录**写进快照，
# 生成的文件是合法 JSON、商品也非空，Rust 加载和全部测试都会通过，没有任何
# 迹象说明那是旧数据。所以最容易出事的那两处判断在这里自己测，不联网：
#
#     python3 crates/apw-core/data/generate.py --self-test


def self_test() -> int:
    real = '{"products":[{"partNumber":"NEW/A","familyType":"iphone17"}]}'
    stale = '{"products":[{"partNumber":"OLD/A","familyType":"iphone12"}]}'

    def bootstrap(body: str) -> bytes:
        return f"window.PRODUCT_SELECTION_BOOTSTRAP = {{ {body} }};".encode()

    # 更长的标识符在前、真正的属性在后：必须跳过前者。少了这道边界检查，
    # 写进快照的会是一批早已下架的型号，用户守着永远不会有货的零件号。
    got = extract(bootstrap(f"legacy_productSelectionData: {stale}, productSelectionData: {real}"))
    assert got and got["products"][0]["partNumber"] == "NEW/A", got

    # 只有更长的标识符时必须找不到，而不是拿旧数据顶上。
    for body in (f"productSelectionDataV2: {stale}", f"legacy_productSelectionData: {stale}"):
        assert extract(bootstrap(body)) is None, body

    # 花括号配平但不是 JSON 的假命中要跳过，接着找真正的属性。
    got = extract(bootstrap(f'tip: "productSelectionData: {{ 看这里 }}", productSelectionData: {real}'))
    assert got and got["products"][0]["partNumber"] == "NEW/A", got

    # 键带不带引号都要认。
    for key in ('productSelectionData:', '"productSelectionData":', "'productSelectionData' :"):
        assert extract(bootstrap(f"{key} {real}")), key

    # part 的采信口径必须与 Rust 的 RawProduct::part_number 一模一样，否则同一台
    # 机器会「在线选得到、离线选不到」。
    def only_part(raw: str):
        return trim(json.loads(raw))["products"]

    # 产品线代号不含斜杠，无论有没有维度都不能当零件号。
    assert not only_part('{"products":[{"part":"IPADPRO11_WI_2025","dimensions":{"a":"b"}}]}')
    # 维度空的、或者全是空白的，都不算维度表形状。
    assert not only_part('{"products":[{"part":"MFCN4CH/B","dimensions":{}}]}')
    assert not only_part('{"products":[{"part":"MFCN4CH/B","dimensions":{"a":"  "}}]}')
    # 真的表壳记录照收。
    kept = only_part('{"products":[{"part":"MFCN4CH/B","dimensions":{"a":"b"}}]}')
    assert len(kept) == 1 and kept[0]["part"] == "MFCN4CH/B", kept

    # 展示文案只留被引用到的取值，且每条只留一个字段。
    trimmed = trim(
        json.loads(
            '{"products":[{"partNumber":"A/A","dimensionColor":"black"}],'
            '"displayValues":{"dimensionColor":{"black":{"value":"黑色","image":"<div/>"},'
            '"unused":{"value":"没人用"}},"prices":{"x":{"value":"$1"}}}}'
        )
    )
    assert trimmed["displayValues"] == {"dimensionColor": {"black": {"value": "黑色"}}}, trimmed

    print("自检通过")
    return 0


def main() -> int:
    here = Path(__file__).resolve().parent
    failed = []

    for locale, (base, accept_language) in REGIONS.items():
        pages = []
        broken = False
        for category, slug in FAMILIES:
            url = f"{base}/shop/{BUY_PATH[category]}/{slug}"
            data = None
            for attempt in range(RETRIES):
                try:
                    data = extract(fetch(url, accept_language))
                except (urllib.error.URLError, OSError) as err:
                    print(f"  {slug}: {err}", file=sys.stderr)
                if data is not None:
                    break
                time.sleep(DELAY_SECONDS * (attempt + 2))

            if data is None:
                failed.append(f"{locale} {slug}")
                broken = True
                print(f"  !! {locale} {slug} 抓取失败", file=sys.stderr)
                continue

            trimmed = trim(data)
            if not trimmed["products"]:
                failed.append(f"{locale} {slug}（无商品）")
                broken = True
                print(f"  !! {locale} {slug} 没有解析出商品", file=sys.stderr)
                continue

            pages.append({"category": category, "family": slug, "data": trimmed})
            print(f"  {locale} {slug}: {len(trimmed['products'])} 个型号")
            time.sleep(DELAY_SECONDS)

        target = here / f"products_{locale}.json"
        if broken:
            # 缺了页就一个字都不写。半份快照比旧快照糟得多：它是合法 JSON、
            # 商品也非空，Rust 加载和测试都会通过，只是「兜底目录里刚好没有
            # 你要的那台机器」—— 而工作区里那份完好的旧快照已经被盖掉了。
            print(f"{target.name}: 有页面缺失，保持原样不覆盖", file=sys.stderr)
            continue

        target.write_text(
            json.dumps(pages, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )
        print(f"{target.name}: {len(pages)} 页，{target.stat().st_size} 字节")

    if failed:
        print("\n抓取失败：" + "、".join(failed), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        raise SystemExit(self_test())
    raise SystemExit(main())
