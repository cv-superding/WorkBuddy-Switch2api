//! 端到端导出一个真实的迁移包，并打印结果供外部（Python zipfile）校验。
//!
//! `cargo run --release -p wb-switch-core --example transfer_export -- [slug|auto] [out.zip] [--with-config]

use std::path::PathBuf;

use wb_switch_core::modules::edition::Edition;
use wb_switch_core::modules::transfer::{self, ExportOptions};

fn mb(n: u64) -> String {
    format!("{:.2} MB", n as f64 / 1024.0 / 1024.0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let with_config = args.iter().any(|a| a == "--with-config");
    // 凭证单独开关：默认不带（包里就是明文 JWT，不该顺手带出去）
    let with_cred = args.iter().any(|a| a == "--with-cred");
    // 凭证单独开关：默认不带（包里就是明文 JWT，不该顺手带出去）
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();

    let edition = Edition::Domestic;
    let scan = transfer::scan_exportable(edition);

    // 挑一个最小但非空的工作区做验证（快）
    let slug = match positional.first() {
        Some(s) if s.as_str() != "auto" => (*s).clone(),
        _ => {
            let ws = scan["workspaces"].as_array().cloned().unwrap_or_default();
            let mut cand: Vec<_> = ws
                .iter()
                .filter(|w| w["sessions"].as_u64().unwrap_or(0) > 0)
                .collect();
            cand.sort_by_key(|w| w["bytes"].as_u64().unwrap_or(u64::MAX));
            match cand.first() {
                Some(w) => w["slug"].as_str().unwrap_or("").to_string(),
                None => {
                    eprintln!("没有可用的工作区");
                    return;
                }
            }
        }
    };
    println!("选中工作区: {slug}");

    let out = positional
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("F:/tmp/transfer-test.zip"));
    if out.exists() {
        std::fs::remove_file(&out).ok();
    }

    let opts = ExportOptions {
        slugs: vec![slug.clone()],
        include_config: with_config,
        include_plugins: false,
        include_file_history: false,
        include_workspace_snapshots: false,
        include_credentials: with_cred,
    };
    println!(
        "选项: config={} credentials={}",
        opts.include_config, opts.include_credentials
    );

    let t0 = std::time::Instant::now();
    match transfer::export_bundle(edition, &opts, &out) {
        Ok(r) => {
            println!("\n导出成功（耗时 {:.1}s）", t0.elapsed().as_secs_f64());
            println!("  包路径   {}", r["path"].as_str().unwrap_or("?"));
            println!("  包大小   {}", mb(r["bytes"].as_u64().unwrap_or(0)));
            println!("  原始体积 {}", mb(r["rawBytes"].as_u64().unwrap_or(0)));
            println!("  会话     {}", r["sessions"].as_u64().unwrap_or(0));
            println!("  附件     {}", r["blobs"].as_u64().unwrap_or(0));
            println!("  条目     {}", r["files"].as_u64().unwrap_or(0));
            if let Some(sk) = r["skipped"].as_array() {
                if !sk.is_empty() {
                    println!("  跳过     {} 个", sk.len());
                    for s in sk.iter().take(5) {
                        println!("           {}", s.as_str().unwrap_or("?"));
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("\n导出失败: {e}");
            std::process::exit(1);
        }
    }
}
