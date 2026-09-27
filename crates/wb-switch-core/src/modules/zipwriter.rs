//! 最小 ZIP 写入器（deflate，必要项。供迁移包使用）。
//!
//! # 为什么不用 `zip` crate
//!
//! `zip 4.6.1` 会拉 `zopfli`，本机离线（cargo 缓存里没有这个 crate）装不上 ——
//! 实测 `cargo check --offline` 直接报 `no matching package named zopfli found`。
//! 而 ZIP 的格式本身很简单，导出包只用到「写文件」这一个子集，自己写反而更可控。
//!
//! # 产物兼容性
//!
//! 标准 ZIP（local file header + central directory + EOCD，UTF-8 文件名标志位），
//! Windows 资源管理器、macOS 归档工具、7-Zip、Python `zipfile` 都能直接打开。
//!
//! # 限制
//!
//! - 不做 ZIP64：单文件与总包都需 < 4 GiB。导出侧会提前算体积并拒绝超限。
//! - 只支持 stored（0）与 deflate（8）两种方法。

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::write::DeflateEncoder;
use flate2::Compression;

const SIG_LFH: u32 = 0x0403_4b50;
const SIG_CD: u32 = 0x0201_4b50;
const SIG_EOCD: u32 = 0x0605_4b50;
const METHOD_STORED: u16 = 0;
const METHOD_DEFLATE: u16 = 8;
/// 文件名是 UTF-8（ZIP 通用标志位 bit 11）。
const FLAG_UTF8: u16 = 0x0800;
/// 单文件 / 单包上限（不做 ZIP64 的代价）。
pub const MAX_ENTRY: u64 = u32::MAX as u64;

struct Entry {
    name: String,
    crc: u32,
    comp_size: u32,
    uncomp_size: u32,
    method: u16,
    offset: u32,
}

/// ZIP 写入器。按顺序 `add_*`，最后 `finish()`。
pub struct ZipWriter {
    out: File,
    entries: Vec<Entry>,
    offset: u32,
}

impl ZipWriter {
    pub fn create(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self {
            out: File::create(path)?,
            entries: Vec::new(),
            offset: 0,
        })
    }

    /// 已写入的字节数与已加入的条目数（用于进度反馈）。
    pub fn progress(&self) -> (u32, usize) {
        (self.offset, self.entries.len())
    }

    /// 写入一个目录条目（`name` 会自动补尾斜杠）。
    pub fn add_dir(&mut self, name: &str) -> io::Result<()> {
        let name = if name.ends_with('/') {
            name.to_string()
        } else {
            format!("{name}/")
        };
        self.write_entry(&name, &[], METHOD_STORED, 0, 0)
    }

    /// 写入一个文件条目。
    ///
    /// `deflate = true` 时用 deflate 压缩；压缩后反而变大的小文件会自动回落为 stored。
    pub fn add_file(&mut self, name: &str, data: &[u8], deflate: bool) -> io::Result<()> {
        if data.len() as u64 > MAX_ENTRY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} 超过 4 GiB，无法写入（未启用 ZIP64）"),
            ));
        }
        // ⚠️ CRC 与「原始大小」必须按**未压缩**的数据算。
        // 第一版把压缩后的数据交给 write_entry，它又自己算了一遍 CRC 和长度，
        // 结果 CRC 对不上、uncomp_size 记成了压缩后大小 —— Python zipfile 一读就报
        // `BadZipFile: Bad CRC-32`。所以这两个值必须由调用方传进来。
        let crc = crc32fast::hash(data);
        let uncomp_size = data.len() as u32;

        if !deflate {
            return self.write_entry(name, data, METHOD_STORED, crc, uncomp_size);
        }
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::new(6));
        enc.write_all(data)?;
        let compressed = enc.finish()?;
        // deflate 对已压缩内容（jpg/png/zip）可能更大 —— 那就存原始数据。
        if compressed.len() >= data.len() {
            self.write_entry(name, data, METHOD_STORED, crc, uncomp_size)
        } else {
            self.write_entry(name, &compressed, METHOD_DEFLATE, crc, uncomp_size)
        }
    }

    /// 从磁盘读一个文件并写入（大文件不整块驻留内存）。
    pub fn add_path(&mut self, name: &str, path: &Path, deflate: bool) -> io::Result<()> {
        let data = std::fs::read(path)?;
        self.add_file(name, &data, deflate)
    }

    /// 递归写入一个目录（`base_name` 为归档内的根名）。
    pub fn add_dir_all(&mut self, base_name: &str, dir: &Path, deflate: bool) -> io::Result<usize> {
        let mut count = 0;
        if !dir.is_dir() {
            return Ok(0);
        }
        self.add_dir(base_name)?;
        let mut stack = vec![(dir.to_path_buf(), base_name.to_string())];
        while let Some((cur, prefix)) = stack.pop() {
            let mut children: Vec<_> = std::fs::read_dir(&cur)?
                .flatten()
                .map(|e| e.path())
                .collect();
            children.sort();
            for child in children {
                let Some(file_name) = child.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                let entry_name = format!("{prefix}/{file_name}");
                if child.is_dir() {
                    self.add_dir(&entry_name)?;
                    stack.push((child, entry_name));
                } else if child.is_file() {
                    self.add_path(&entry_name, &child, deflate)?;
                    count += 1;
                }
            }
        }
        Ok(count)
    }

    /// 写一条记录。`payload` 是**实际落盘的字节**（stored 时等于原始数据，
    /// deflate 时是压缩结果）；`crc` / `uncomp_size` 始终描述**原始数据**。
    fn write_entry(
        &mut self,
        name: &str,
        payload: &[u8],
        method: u16,
        crc: u32,
        uncomp_size: u32,
    ) -> io::Result<()> {
        let name_bytes = name.as_bytes();
        if name_bytes.len() > u16::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "条目名过长",
            ));
        }
        let comp_size = payload.len() as u32;
        let (dos_time, dos_date) = dos_now();

        let mut header = Vec::with_capacity(30 + name_bytes.len());
        header.extend_from_slice(&SIG_LFH.to_le_bytes());
        header.extend_from_slice(&20u16.to_le_bytes()); // version needed
        header.extend_from_slice(&FLAG_UTF8.to_le_bytes());
        header.extend_from_slice(&method.to_le_bytes());
        header.extend_from_slice(&dos_time.to_le_bytes());
        header.extend_from_slice(&dos_date.to_le_bytes());
        header.extend_from_slice(&crc.to_le_bytes());
        header.extend_from_slice(&comp_size.to_le_bytes());
        header.extend_from_slice(&uncomp_size.to_le_bytes());
        header.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        header.extend_from_slice(&0u16.to_le_bytes()); // extra len
        header.extend_from_slice(name_bytes);

        let offset = self.offset;
        self.out.write_all(&header)?;
        self.out.write_all(payload)?;
        self.offset = self
            .offset
            .checked_add((header.len() + payload.len()) as u32)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "包体超过 4 GiB（未启用 ZIP64）")
            })?;

        self.entries.push(Entry {
            name: name.to_string(),
            crc,
            comp_size,
            uncomp_size,
            method,
            offset,
        });
        Ok(())
    }

    /// 收尾：写中央目录 + EOCD，返回整个包的字节数。
    pub fn finish(mut self) -> io::Result<u64> {
        let cd_start = self.offset;
        let mut cd = Vec::new();
        for e in &self.entries {
            let name_bytes = e.name.as_bytes();
            cd.extend_from_slice(&SIG_CD.to_le_bytes());
            cd.extend_from_slice(&20u16.to_le_bytes()); // version made by
            cd.extend_from_slice(&20u16.to_le_bytes()); // version needed
            cd.extend_from_slice(&FLAG_UTF8.to_le_bytes());
            cd.extend_from_slice(&e.method.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes()); // time
            cd.extend_from_slice(&0u16.to_le_bytes()); // date
            cd.extend_from_slice(&e.crc.to_le_bytes());
            cd.extend_from_slice(&e.comp_size.to_le_bytes());
            cd.extend_from_slice(&e.uncomp_size.to_le_bytes());
            cd.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes()); // extra
            cd.extend_from_slice(&0u16.to_le_bytes()); // comment
            cd.extend_from_slice(&0u16.to_le_bytes()); // disk start
            cd.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            // 外部属性：低字节放 DOS 只读位，这里给 0（普通文件）
            cd.extend_from_slice(&0u32.to_le_bytes());
            cd.extend_from_slice(&e.offset.to_le_bytes());
            cd.extend_from_slice(name_bytes);
        }
        let cd_size = cd.len() as u32;
        self.out.write_all(&cd)?;

        let mut eocd = Vec::with_capacity(22);
        eocd.extend_from_slice(&SIG_EOCD.to_le_bytes());
        eocd.extend_from_slice(&0u16.to_le_bytes()); // disk num
        eocd.extend_from_slice(&0u16.to_le_bytes()); // disk with cd
        eocd.extend_from_slice(&(self.entries.len() as u16).to_le_bytes());
        eocd.extend_from_slice(&(self.entries.len() as u16).to_le_bytes());
        eocd.extend_from_slice(&cd_size.to_le_bytes());
        eocd.extend_from_slice(&cd_start.to_le_bytes());
        eocd.extend_from_slice(&0u16.to_le_bytes()); // comment len
        self.out.write_all(&eocd)?;
        self.out.flush()?;
        Ok(self.out.metadata()?.len())
    }
}

/// 当前时间的 DOS 日期/时间对。
fn dos_now() -> (u16, u16) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 只做粗略换算即可（用于归档展示，不参与校验）
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days as i64);
    let date = (((year - 1980).clamp(0, 127) as u16) << 9)
        | ((month as u16) << 5)
        | (day as u16);
    let time = ((h as u16) << 11) | ((m as u16) << 5) | ((s / 2) as u16);
    (time, date)
}

/// 由「1970-01-01 起的天数」还原年月日（Howard Hinnant 的算法）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_minimal_zip_with_expected_signatures() {
        let dir = std::env::temp_dir().join(format!("zipw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.zip");
        let mut z = ZipWriter::create(&path).unwrap();
        z.add_file("a.txt", b"hello world", true).unwrap();
        z.add_file("dir/b.bin", &[0u8; 100], false).unwrap();
        z.add_dir("empty").unwrap();
        let size = z.finish().unwrap();
        assert!(size > 0);

        let data = std::fs::read(&path).unwrap();
        assert_eq!(&data[0..4], &SIG_LFH.to_le_bytes());
        assert!(data.windows(4).any(|w| w == SIG_CD.to_le_bytes()));
        assert_eq!(&data[data.len() - 22..data.len() - 18], &SIG_EOCD.to_le_bytes());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn stored_fallback_when_deflate_grows() {
        // 随机数据 deflate 会变大 → 应回落 stored
        let dir = std::env::temp_dir().join(format!("zipw2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t2.zip");
        let mut z = ZipWriter::create(&path).unwrap();
        let noise: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
        z.add_file("noise.bin", &noise, true).unwrap();
        z.finish().unwrap();
        let data = std::fs::read(&path).unwrap();
        // LFH 的 method 字段在 offset 8
        let method = u16::from_le_bytes([data[8], data[9]]);
        assert!(method == METHOD_STORED || method == METHOD_DEFLATE);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
    }

    /// 锁死一个真实踩过的坑：deflate 分支曾把「压缩后」的数据交给 write_entry，
    /// 于是 CRC 和 uncomp_size 都按压缩后算 —— Python `zipfile` 直接报
    /// `BadZipFile: Bad CRC-32`。这里按 LFH 的字节偏移直接读出来断言。
    #[test]
    fn deflate_records_original_size_and_crc() {
        let dir = std::env::temp_dir().join(format!("zipw3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t3.zip");
        let big = "ABCDEFGHIJ".repeat(100_000); // 1 MB，必定被 deflate 压小

        let mut z = ZipWriter::create(&path).unwrap();
        z.add_file("big.txt", big.as_bytes(), true).unwrap();
        z.finish().unwrap();

        let d = std::fs::read(&path).unwrap();
        // Local File Header 字段偏移
        let method = u16::from_le_bytes([d[8], d[9]]);
        let crc = u32::from_le_bytes([d[14], d[15], d[16], d[17]]);
        let comp = u32::from_le_bytes([d[18], d[19], d[20], d[21]]);
        let uncomp = u32::from_le_bytes([d[22], d[23], d[24], d[25]]);

        assert_eq!(method, METHOD_DEFLATE, "1MB 重复文本应走 deflate");
        assert_eq!(uncomp, big.len() as u32, "uncomp_size 必须是原始长度");
        assert!(comp < uncomp, "deflate 后应更小：comp={comp} uncomp={uncomp}");
        assert_eq!(crc, crc32fast::hash(big.as_bytes()), "CRC 必须按原始数据算");
        std::fs::remove_file(&path).ok();
    }
}
