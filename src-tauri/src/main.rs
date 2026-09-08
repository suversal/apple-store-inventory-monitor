// 发布构建时不要附带控制台窗口：Windows 上双击 exe 会额外弹出一个黑框，
// 看起来像是运行出错了。调试构建保留，方便看日志。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    apple_store_inventory_monitor_lib::run()
}
