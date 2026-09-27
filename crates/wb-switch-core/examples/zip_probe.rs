//! 生成一个 zip 供外部（Python `zipfile`）校验。
//!
//! 用法：`cargo run --release -p wb-switch-core --example zip_probe -- <out.zip>`
//!
//! 覆盖：普通文本、中文路径、二进制、目录条目、可压缩的大文本。

use std::path::PathBuf;

use wb_switch_core::modules::zipwriter::ZipWriter;

fn main() {
    let out: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("F:/tmp/zip-probe.zip"));

    let mut z = ZipWriter::create(&out).expect("create");

    z.add_file("a.txt", b"hello world\n", true).expect("a.txt");
    z.add_file(
        "中文目录/中文文件.txt",
        "这是 UTF-8 文件名与内容的测试。\n".as_bytes(),
        true,
    )
    .expect("zh");

    // 伪随机数据 —— deflate 会变大，应自动回落 stored
    let noise: Vec<u8> = (0..4096u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    z.add_file("bin/noise.bin", &noise, true).expect("noise");

    // 高可压缩：1 MB 的重复文本
    let big = "ABCDEFGHIJ".repeat(100_000);
    z.add_file("big/repeat.txt", big.as_bytes(), true).expect("big");

    z.add_dir("empty-dir").expect("dir");

    // 深层目录树
    z.add_file("deep/a/b/c/d.txt", b"deep\n", true).expect("deep");

    let size = z.finish().expect("finish");
    println!("out={}", out.display());
    println!("size={size}");
    println!("noise_len={} big_len={}", noise.len(), big.len());
}
