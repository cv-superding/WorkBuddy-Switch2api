//! 极简 ZIP **读取器**（只读，无 ZIP64）。
//!
//! 与 [`super::zipwriter`] 对称：那边写、这边读。之所以不引入 `zip` crate ——
//! 它的默认特性会拉 `zopfli`，本机离线装不上（见 zipwriter 头部注释）。
//!
//! 支持：
//! - `stored`(0) 与 `deflate`(8) 两种方法（ZIP 里 99% 是这两种）
//! - UTF-8 条目名（flag bit 11）；其余编码按原字节尽力还原
//! - **每次读取都校验 CRC32** —— 包损坏必须响亮地失败，
//!   不能把半个文件悄悄导进用户的数据目录
//! - 解压体积上限，防压缩炸弹
//!
//! **不支持**：ZIP64、加密、多卷、zstd/bzip2/lzma 方法。遇到时返回明确错误，
//! 而不是猜一个可能错的结果。

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use flate2::read::DeflateDecoder;

const SIG_LFH: u32 = 0x0403_4b50;
const SIG_CD: u32 = 0x0201_4b50;
const SIG_EOCD: u32 = 0x0605_4b50;
const SIG_ZIP64_EOCD_LOC: u32 = 0x0706_4b50;
const METHOD_STORED: u16 = 0;
const METHOD_DEFLATE: u16 = 8;

/// 单个条目的解压上限（512 MiB）。
///
/// 包是我们自己产的，正常远小于此；但读**别人的**包时必须有这道闸 ——
/// deflate 能把 1 KB 炸成 1 GB。
pub const DEFAULT_MAX_ENTRY: u64 = 512 * 1024 * 1024;

fn bad<T>(msg: impl Into<String>) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidData, msg.into()))
}

#[derive(Debug, Clone)]
pub struct ZipEntry {
    pub name: String,
    pub method: u16,
    pub crc: u32,
    pub comp_size: u64,
    /// 中央目录里声明的原始大小（仅作参考；实际以解出来的为准）。
    pub uncomp_size: u64,
    local_offset: u64,
}

impl ZipEntry {
    pub fn is_dir(&self) -> bool {
        self.name.ends_with('/')
    }
}

pub struct ZipReader {
    path: PathBuf,
    /// 条目顺序保留（manifest 在最前，便于流式读时先看到）。
    entries: Vec<ZipEntry>,
    index: BTreeMap<String, usize>,
}

impl ZipReader {
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut f = File::open(path)?;
        let size = f.metadata()?.len();
        if size < 22 {
            return bad("文件小于 22 字节，不可能是 zip");
        }

        // ---- 1) 从尾部找 EOCD（注释最长 64 KiB）----
        let tail_len = size.min(65_557) as usize;
        let mut tail = vec![0u8; tail_len];
        f.seek(SeekFrom::Start(size - tail_len as u64))?;
        f.read_exact(&mut tail)?;

        let mut eocd_at = None;
        // 从后往前找第一个签名（标准做法：取最后一个，因为注释里可能含伪签名）。
        for i in (0..tail.len().saturating_sub(21)).rev() {
            if u32::from_le_bytes([tail[i], tail[i + 1], tail[i + 2], tail[i + 3]]) == SIG_EOCD {
                eocd_at = Some(i);
                break;
            }
        }
        let e = eocd_at.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "找不到 EOCD 标记 —— 不是 zip 文件，或者下载不完整",
            )
        })?;
        let r16 = |o: usize| u16::from_le_bytes([tail[o], tail[o + 1]]) as u64;
        let r32 = |o: usize| u32::from_le_bytes([tail[o], tail[o + 1], tail[o + 2], tail[o + 3]]) as u64;

        // ZIP64 探测器：EOCD 前 20 字节若是 locator 签名，说明用了 ZIP64。
        if e >= 20 {
            let z = e - 20;
            if u32::from_le_bytes([tail[z], tail[z + 1], tail[z + 2], tail[z + 3]]) == SIG_ZIP64_EOCD_LOC
            {
                return bad("这个包用了 ZIP64（超过 4 GiB 或条目数超 65535），当前版本不支持");
            }
        }

        let total = r16(e + 10);
        let cd_size = r32(e + 12);
        let cd_off = r32(e + 16);
        if cd_off == 0xFFFF_FFFF || cd_size == 0xFFFF_FFFF {
            return bad("包内偏移写成 ZIP64 哨兵值，当前版本不支持");
        }

        // ---- 2) 读中央目录 ----
        if cd_off + cd_size > size {
            return bad("中央目录越界 —— 文件被截断");
        }
        let mut cd = vec![0u8; cd_size as usize];
        f.seek(SeekFrom::Start(cd_off))?;
        f.read_exact(&mut cd)?;

        let mut entries: Vec<ZipEntry> = Vec::with_capacity(total as usize);
        let mut p = 0usize;
        while p + 46 <= cd.len() {
            let sig = u32::from_le_bytes([cd[p], cd[p + 1], cd[p + 2], cd[p + 3]]);
            if sig != SIG_CD {
                break;
            }
            let g16 = |o: usize| u16::from_le_bytes([cd[p + o], cd[p + o + 1]]) as usize;
            let g32 = |o: usize| u32::from_le_bytes([cd[p + o], cd[p + o + 1], cd[p + o + 2], cd[p + o + 3]]);
            let method = u16::from_le_bytes([cd[p + 10], cd[p + 11]]);
            let crc = g32(16);
            let comp_size = g32(20) as u64;
            let uncomp_size = g32(24) as u64;
            let nlen = g16(28);
            let elen = g16(30);
            let clen = g16(32);
            let local_offset = g32(42) as u64;

            if p + 46 + nlen + elen + clen > cd.len() {
                break;
            }
            let name_bytes = &cd[p + 46..p + 46 + nlen];
            let name = String::from_utf8_lossy(name_bytes).to_string();

            entries.push(ZipEntry {
                name,
                method,
                crc,
                comp_size,
                uncomp_size,
                local_offset,
            });
            p += 46 + nlen + elen + clen;
        }
        if entries.is_empty() {
            return bad("包是空的（中央目录里没有条目）");
        }

        let index = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.name.clone(), i))
            .collect();

        Ok(Self {
            path: path.to_path_buf(),
            entries,
            index,
        })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entries(&self) -> &[ZipEntry] {
        &self.entries
    }

    /// 全部条目名（保留包内顺序）。
    pub fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.name.as_str()).collect()
    }

    /// 以某前缀开头的条目名。
    pub fn names_with_prefix<'a>(&'a self, prefix: &str) -> Vec<&'a str> {
        self.entries
            .iter()
            .filter(|e| e.name.starts_with(prefix))
            .map(|e| e.name.as_str())
            .collect()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.index.contains_key(name)
    }

    pub fn entry(&self, name: &str) -> Option<&ZipEntry> {
        self.index.get(name).map(|i| &self.entries[*i])
    }

    /// 取一条目的**压缩态**原始字节（不做解压）。
    fn raw(&self, e: &ZipEntry) -> io::Result<Vec<u8>> {
        let mut f = File::open(&self.path)?;
        f.seek(SeekFrom::Start(e.local_offset))?;

        let mut h = [0u8; 30];
        f.read_exact(&mut h)?;
        if u32::from_le_bytes([h[0], h[1], h[2], h[3]]) != SIG_LFH {
            return bad(format!("条目 {} 的本地头签名不对", e.name));
        }
        // ⚠️ 必须用**本地头**的 name/extra 长度来定位数据起点：
        // 中央目录与本地头的 extra 字段长度允许不同（Zip64/Zip32 扩展常见）。
        let nlen = u16::from_le_bytes([h[26], h[27]]) as i64;
        let elen = u16::from_le_bytes([h[28], h[29]]) as i64;
        f.seek(SeekFrom::Current(nlen + elen))?;

        if e.comp_size > DEFAULT_MAX_ENTRY * 4 {
            return bad(format!("条目 {} 压缩态就有 {} 字节，拒绝读入内存", e.name, e.comp_size));
        }
        let mut buf = vec![0u8; e.comp_size as usize];
        f.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// 读一个条目并按 CRC32 校验。
    pub fn read(&self, name: &str) -> io::Result<Vec<u8>> {
        self.read_limited(name, DEFAULT_MAX_ENTRY)
    }

    /// 同 [`Self::read`]，但限制解压后的体积。
    pub fn read_limited(&self, name: &str, max: u64) -> io::Result<Vec<u8>> {
        let e = self
            .entry(name)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("包里没有 {name}"))
            })?
            .clone();
        if e.uncomp_size > max {
            return bad(format!(
                "条目 {} 解压后 {} 字节，超过上限 {} 字节",
                name, e.uncomp_size, max
            ));
        }
        let raw = self.raw(&e)?;
        let data = match e.method {
            METHOD_STORED => raw,
            METHOD_DEFLATE => {
                let mut d = DeflateDecoder::new(&raw[..]);
                let mut out: Vec<u8> = Vec::with_capacity(e.uncomp_size.min(max) as usize);
                let mut chunk = [0u8; 64 * 1024];
                loop {
                    let n = d.read(&mut chunk)?;
                    if n == 0 {
                        break;
                    }
                    if out.len() as u64 + n as u64 > max {
                        return bad(format!("{name} 解压超过上限 {max} 字节（疑似压缩炸弹）"));
                    }
                    out.extend_from_slice(&chunk[..n]);
                }
                out
            }
            m => {
                return bad(format!(
                    "{name} 用了不支持的压缩方法 {m}（只支持 stored=0 / deflate=8）"
                ))
            }
        };
        let crc = crc32fast::hash(&data);
        if crc != e.crc {
            return bad(format!(
                "{name} 的 CRC32 不匹配：包内声明 {:08x}，实际算出 {:08x} —— 文件可能已损坏",
                e.crc, crc
            ));
        }
        Ok(data)
    }

    /// 读成 UTF-8 文本（非法字节按替换字符处理，不失败）。
    pub fn read_text(&self, name: &str) -> io::Result<String> {
        let b = self.read(name)?;
        Ok(String::from_utf8_lossy(&b).to_string())
    }

    /// 读成 JSON。
    pub fn read_json(&self, name: &str) -> io::Result<serde_json::Value> {
        let t = self.read_text(name)?;
        serde_json::from_str(&t).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{name} 不是合法 JSON：{err}"),
            )
        })
    }

    /// 把一条目落到 `dest`（自动建父目录）。返回写入字节数。
    ///
    /// 只负责写文件，**不做路径拼接** —— 落点由调用方给出，
    /// 这样 `blobs/{hh}/{hash}` 这类规则可以自己控制。
    pub fn extract_to(&self, name: &str, dest: &Path) -> io::Result<u64> {
        let data = self.read(name)?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(dest, &data)?;
        Ok(data.len() as u64)
    }
}

/// 把归档里的相对路径拼到根目录下，**拒绝目录穿越**。
///
/// 读别人的包时必须过这一关：`../../Windows/System32/...` 这种条目
/// 一旦直接拼路径就是任意文件写入。
pub fn safe_join(root: &Path, rel: &str) -> io::Result<PathBuf> {
    let rel = rel.replace('\\', "/");
    let trimmed = rel.trim_start_matches('/');
    if trimmed.is_empty() {
        return bad("条目名为空");
    }
    let mut out = root.to_path_buf();
    for part in trimmed.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return bad(format!("条目名含上级目录引用，已拒绝：{rel}"));
        }
        // Windows 上还要挡掉盘符与冒号（`C:foo` 会被当成相对盘路径）。
        if part.contains(':') {
            return bad(format!("条目名含非法字符，已拒绝：{rel}"));
        }
        out.push(part);
    }
    Ok(out)
}

/// 递归建目录（不依赖 walkdir，保持零额外依赖）。
pub fn ensure_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::zipwriter::ZipWriter;

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("wb-zipreader-{}-{}", name, std::process::id()));
        p
    }

    /// 关键回归：**读回自己写的包**，逐条比对内容与目录。
    #[test]
    fn reads_back_own_archive() {
        let p = tmp("roundtrip.zip");
        let _ = std::fs::remove_file(&p);

        let big: Vec<u8> = b"abcdefgh".iter().cycle().take(200_000).copied().collect();
        let noise: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();

        {
            let mut z = ZipWriter::create(&p).unwrap();
            z.add_dir("dir").unwrap();
            z.add_file("dir/a.txt", b"hello\n", true).unwrap();
            z.add_file("中文/文件.txt", "中文内容\n".as_bytes(), true).unwrap();
            z.add_file("big.bin", &big, true).unwrap();
            z.add_file("noise.bin", &noise, false).unwrap();
            z.add_file("empty.txt", b"", true).unwrap();
            z.add_dir("only-dir/").unwrap();
            z.finish().unwrap();
        }

        let r = ZipReader::open(&p).unwrap();
        assert_eq!(r.len(), 7, "条目数：{}", r.len());
        assert_eq!(r.read("dir/a.txt").unwrap(), b"hello\n");
        assert_eq!(
            String::from_utf8(r.read("中文/文件.txt").unwrap()).unwrap(),
            "中文内容\n"
        );
        assert_eq!(r.read("big.bin").unwrap(), big);
        assert_eq!(r.read("noise.bin").unwrap(), noise);
        assert_eq!(r.read("empty.txt").unwrap(), b"");
        assert!(r.entry("dir/").unwrap().is_dir());
        assert!(r.entry("only-dir/").unwrap().is_dir());
        assert!(!r.contains("nope.txt"));

        // 压缩确实生效了（big 高度可压）
        let e = r.entry("big.bin").unwrap();
        assert!(e.comp_size < 10_000, "big.bin 压缩后 {} 字节，deflate 没生效？", e.comp_size);
        assert_eq!(e.uncomp_size, 200_000);

        let _ = std::fs::remove_file(&p);
    }

    /// 损坏的包必须报错，而不是返回错误数据。
    #[test]
    fn detects_corruption() {
        let p = tmp("corrupt.zip");
        let _ = std::fs::remove_file(&p);
        {
            let mut z = ZipWriter::create(&p).unwrap();
            z.add_file("a.txt", &vec![b'x'; 5000], true).unwrap();
            z.finish().unwrap();
        }
        // 改掉数据区里的一个字节（deflate 流在本地头之后，跳过 30+名长）。
        let mut bytes = std::fs::read(&p).unwrap();
        let flip = 30 + 5 + 40; // 名长 5、头 30，再往里 40 字节 —— 稳落在压缩数据里
        bytes[flip] ^= 0xFF;
        std::fs::write(&p, &bytes).unwrap();

        let r = ZipReader::open(&p).unwrap();
        let err = r.read("a.txt").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn rejects_traversal() {
        let root = Path::new("C:/base");
        assert!(safe_join(root, "ok/a.txt").is_ok());
        assert!(safe_join(root, "a\\b.txt").is_ok());
        assert_eq!(safe_join(root, "ok/a.txt").unwrap(), root.join("ok").join("a.txt"));
        assert!(safe_join(root, "../evil.txt").is_err());
        assert!(safe_join(root, "a/../../evil.txt").is_err());
        assert!(safe_join(root, "C:/evil.txt").is_err());
    }

    #[test]
    fn rejects_non_zip() {
        let p = tmp("notzip.bin");
        std::fs::write(&p, b"this is definitely not a zip file, just text").unwrap();
        // ZipReader 没有 Debug（含文件句柄），所以不能用 unwrap_err。
        match ZipReader::open(&p) {
            Ok(_) => panic!("纯文本被当成了 zip"),
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::InvalidData, "错误类型: {e}"),
        }
        let _ = std::fs::remove_file(&p);
    }
}
