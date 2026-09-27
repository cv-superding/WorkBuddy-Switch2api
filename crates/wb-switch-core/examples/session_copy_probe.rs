//! 账号间复制会话的端到端探针。
//!
//! 用法：
//!   cargo run --release -p wb-switch-core --example session_copy_probe -- <cn|ai> <cid> <src_uid> <dst_uid>
//!
//! ⚠️ 会真的读写数据目录。验证时把 `WB_SWITCH_HOME_OVERRIDE` 指向临时目录。

use wb_switch_core::modules::config::home_dir;
use wb_switch_core::modules::edition::Edition;
use wb_switch_core::modules::session::copy_session_to_user_for;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 4 {
        eprintln!("用法: session_copy_probe <cn|ai> <cid> <src_uid> <dst_uid>");
        std::process::exit(2);
    }
    let edition = match args[0].as_str() {
        "ai" | "international" => Edition::International,
        _ => Edition::Domestic,
    };
    println!("home   = {}", home_dir().display());
    println!("dataDir= {}", edition.data_dir().display());
    match copy_session_to_user_for(edition, &args[1], &args[2], &args[3]) {
        Ok(v) => {
            // 把新 id 单独打一行，方便脚本抓
            println!("NEWID={}", v.get("newId").and_then(|x| x.as_str()).unwrap_or(""));
            println!("{}", serde_json::to_string_pretty(&v).unwrap());
        }
        Err(e) => {
            eprintln!("复制失败: {e}");
            std::process::exit(1);
        }
    }
}
