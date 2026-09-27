//! 导入端到端探针。
//!
//! 用法：
//!   cargo run --release -p wb-switch-core --example transfer_import -- <zip> <cn|ai> preview
//!   cargo run --release -p wb-switch-core --example transfer_import -- <zip> <cn|ai> run [--overwrite] [--cred] [--no-config]
//!
//! ⚠️ 会真的写数据目录。验证时把 `USERPROFILE` 指向一个临时目录来隔离。

use std::path::{Path, PathBuf};

use wb_switch_core::modules::edition::Edition;
use wb_switch_core::modules::transfer::{import_bundle, preview_bundle, ImportOptions};

fn pick_edition(key: &str) -> Edition {
    // 接受 key（domestic / international）与短别名（cn / ai）。
    let key = match key {
        "cn" => "domestic",
        "ai" => "international",
        other => other,
    };
    for e in Edition::ALL {
        if e.key() == key {
            return e;
        }
    }
    eprintln!("未知档位 {key}（可用 domestic|cn / international|ai）");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!("用法: transfer_import <zip> <domestic|international> <preview|run> [--overwrite] [--cred] [--no-config]");
        std::process::exit(2);
    }
    let zip = PathBuf::from(&args[0]);
    let edition = pick_edition(&args[1]);
    let mode = args[2].as_str();
    let flags = &args[3..];
    let has = |f: &str| flags.iter().any(|a| a == f);

    println!("home    = {}", wb_switch_core::modules::config::home_dir().display());
    println!("dataDir = {}", edition.data_dir().display());
    println!("包      = {}\n", zip.display());

    if mode == "preview" {
        match preview_bundle(edition, &zip) {
            Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap()),
            Err(e) => {
                eprintln!("预览失败: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let opts = ImportOptions {
        overwrite: has("--overwrite"),
        apply_credentials: has("--cred"),
        apply_config: !has("--no-config"),
        ..Default::default()
    };
    match import_bundle(edition, &zip, &opts) {
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v).unwrap());
            println!("\n备份目录: {}", v["backup"]["dir"].as_str().unwrap_or(""));
        }
        Err(e) => {
            eprintln!("导入失败: {e}");
            std::process::exit(1);
        }
    }
    let _ = Path::new(".");
}
