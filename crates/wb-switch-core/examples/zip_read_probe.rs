//! 互操作探针：读一个**外部工具**（Python zipfile / 资源管理器 / 7-Zip）产出的 zip，
//! 打印条目清单与内容摘要。
//!
//! 用法：cargo run --release -p wb-switch-core --example zip_read_probe -- <zip 路径>

use std::path::PathBuf;
use wb_switch_core::modules::zipreader::ZipReader;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("用法: zip_read_probe <zip 路径>");
        std::process::exit(2);
    };
    let p = PathBuf::from(path);

    let r = match ZipReader::open(&p) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("打开失败: {e}");
            std::process::exit(1);
        }
    };
    println!("包: {}  ({} 字节)", p.display(), std::fs::metadata(&p).unwrap().len());
    println!("条目 {} 个\n", r.len());
    println!("{:<40} {:>10} {:>10} {:>6}  状态", "名称", "原始", "压缩后", "方法");

    let mut total_uncomp = 0u64;
    let mut bad = 0;
    for e in r.entries() {
        let m = match e.method {
            0 => "stored",
            8 => "deflate",
            _ => "其他",
        };
        let status = if e.is_dir() {
            "目录".to_string()
        } else {
            match r.read(&e.name) {
                Ok(d) => {
                    total_uncomp += d.len() as u64;
                    format!("OK {}", d.len())
                }
                Err(err) => {
                    bad += 1;
                    format!("❌ {err}")
                }
            }
        };
        let shown = if e.name.len() > 38 { format!("…{}", &e.name[e.name.len() - 37..]) } else { e.name.clone() };
        println!(
            "{:<40} {:>10} {:>10} {:>6}  {}",
            shown, e.uncomp_size, e.comp_size, m, status
        );
    }

    println!("\n解出总字节: {total_uncomp}");
    println!("失败的条目: {bad}");
    println!(
        "\n结论: {}",
        if bad == 0 {
            "✅ 全部条目读取成功，CRC 通过"
        } else {
            "❌ 有条目读取失败"
        }
    );
    std::process::exit(if bad == 0 { 0 } else { 1 });
}
