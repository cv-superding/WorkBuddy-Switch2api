//! 打印两个版本的「可导出内容」扫描结果。
//!
//! `cargo run --release -p wb-switch-core --example transfer_scan`

use wb_switch_core::modules::edition::Edition;
use wb_switch_core::modules::transfer;

fn mb(n: u64) -> String {
    let f = n as f64;
    if f >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GB", f / 1024.0 / 1024.0 / 1024.0)
    } else if f >= 1024.0 * 1024.0 {
        format!("{:.1} MB", f / 1024.0 / 1024.0)
    } else if f >= 1024.0 {
        format!("{:.1} KB", f / 1024.0)
    } else {
        format!("{n} B")
    }
}

fn main() {
    for ed in [Edition::Domestic, Edition::International] {
        let v = transfer::scan_exportable(ed);
        println!("{}", "=".repeat(72));
        println!(
            "{}  ({})  uid={}",
            v["editionLabel"].as_str().unwrap_or("?"),
            v["dataDir"].as_str().unwrap_or("?"),
            v["uid"].as_str().unwrap_or("(未登录)")
        );

        let ws = v["workspaces"].as_array().cloned().unwrap_or_default();
        println!(
            "\n工作区 {} 个（会话文件 {} 个）：",
            v["workspaceCount"].as_u64().unwrap_or(0),
            v["sessionFiles"].as_u64().unwrap_or(0)
        );
        for w in ws.iter().take(8) {
            println!(
                "   {:>5} 会话  {:>10}  {:>6} 文件   {}",
                w["sessions"].as_u64().unwrap_or(0),
                mb(w["bytes"].as_u64().unwrap_or(0)),
                w["files"].as_u64().unwrap_or(0),
                w["title"].as_str().unwrap_or("?")
            );
        }
        if ws.len() > 8 {
            println!("   … 另 {} 个工作区", ws.len() - 8);
        }

        println!("\n配置项：");
        for c in v["config"].as_array().cloned().unwrap_or_default() {
            let opt = if c["optional"].as_bool().unwrap_or(false) {
                " [可选]"
            } else {
                ""
            };
            println!(
                "   {:<8} {:>10}  {:>6} 文件   {}{}",
                c["key"].as_str().unwrap_or("?"),
                mb(c["bytes"].as_u64().unwrap_or(0)),
                c["files"].as_u64().unwrap_or(0),
                c["label"].as_str().unwrap_or("?"),
                opt
            );
        }

        let e = &v["extras"];
        println!("\n附属数据：");
        for k in [
            "blobs",
            "fileHistory",
            "tasks",
            "artifactIndex",
            "workspaceSnapshots",
        ] {
            println!(
                "   {:<20} {:>10}  {:>7} 文件",
                k,
                mb(e[k]["bytes"].as_u64().unwrap_or(0)),
                e[k]["files"].as_u64().unwrap_or(0)
            );
        }
        println!(
            "   {:<20} {:>10}",
            "database",
            mb(e["database"]["bytes"].as_u64().unwrap_or(0))
        );
        println!();
    }
}
