//! Small classic ZIP archives: deflated entries for bug reports, and reading stored or deflated
//! entries for release archives (upstream `utils/zip.ts`).

use std::io::{Read, Write};

use flate2::Compression;
use flate2::Crc;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZipEntry {
    pub name: String,
    pub data: Vec<u8>,
}

impl ZipEntry {
    pub fn new(name: impl Into<String>, data: impl Into<Vec<u8>>) -> Self {
        Self { name: name.into(), data: data.into() }
    }
}

/// MS-DOS time and date of a Unix time in seconds (UTC).
fn dos_date_time(unix_seconds: i64) -> (u16, u16) {
    let days = unix_seconds.div_euclid(86_400);
    let seconds = unix_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let time = ((seconds / 3600) << 11) | (((seconds % 3600) / 60) << 5) | ((seconds % 60) >> 1);
    let date = ((year.max(1980) - 1980) << 9) | (month << 5) | day;
    (time as u16, date as u16)
}

/// Year, month, and day of a count of days since 1970-01-01.
pub(crate) fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = Crc::new();
    crc.update(data);
    crc.sum()
}

fn put16(buffer: &mut Vec<u8>, value: u16) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

fn put32(buffer: &mut Vec<u8>, value: u32) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

/// A ZIP archive with every entry deflated and UTF-8 names.
pub fn create_zip_archive(entries: &[ZipEntry]) -> std::io::Result<Vec<u8>> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (time, date) = dos_date_time(now as i64);
    let mut files = Vec::new();
    let mut directory = Vec::new();
    for entry in entries {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&entry.data)?;
        let compressed = encoder.finish()?;
        let checksum = crc32(&entry.data);
        let offset = files.len() as u32;
        let name = entry.name.as_bytes();
        let header = |buffer: &mut Vec<u8>, central: bool| {
            put32(buffer, if central { 0x0201_4b50 } else { 0x0403_4b50 });
            if central {
                put16(buffer, 20);
            }
            put16(buffer, 20);
            put16(buffer, 0x0800);
            put16(buffer, 8);
            put16(buffer, time);
            put16(buffer, date);
            put32(buffer, checksum);
            put32(buffer, compressed.len() as u32);
            put32(buffer, entry.data.len() as u32);
            put16(buffer, name.len() as u16);
            put16(buffer, 0);
        };
        header(&mut files, false);
        files.extend_from_slice(name);
        files.extend_from_slice(&compressed);
        header(&mut directory, true);
        directory.extend_from_slice(&[0; 10]);
        put32(&mut directory, offset);
        directory.extend_from_slice(name);
    }
    let mut archive = files;
    let directory_offset = archive.len() as u32;
    archive.extend_from_slice(&directory);
    put32(&mut archive, 0x0605_4b50);
    put32(&mut archive, 0);
    put16(&mut archive, entries.len() as u16);
    put16(&mut archive, entries.len() as u16);
    put32(&mut archive, directory.len() as u32);
    put32(&mut archive, directory_offset);
    put16(&mut archive, 0);
    Ok(archive)
}

pub fn write_zip_archive(path: &std::path::Path, entries: &[ZipEntry]) -> std::io::Result<()> {
    std::fs::write(path, create_zip_archive(entries)?)
}

fn read16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn read32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Entries of a ZIP archive via its central directory; stored and deflated entries only.
pub fn read_zip_archive(bytes: &[u8]) -> Result<Vec<ZipEntry>, String> {
    let invalid = || "Invalid zip archive".to_string();
    let search_start = bytes.len().saturating_sub(22 + 65_535);
    let end = (search_start..=bytes.len().saturating_sub(22))
        .rev()
        .find(|&at| read32(bytes, at) == Some(0x0605_4b50))
        .ok_or_else(invalid)?;
    let count = read16(bytes, end + 10).ok_or_else(invalid)? as usize;
    let mut at = read32(bytes, end + 16).ok_or_else(invalid)? as usize;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        if read32(bytes, at) != Some(0x0201_4b50) {
            return Err(invalid());
        }
        let method = read16(bytes, at + 10).ok_or_else(invalid)?;
        let checksum = read32(bytes, at + 16).ok_or_else(invalid)?;
        let compressed_size = read32(bytes, at + 20).ok_or_else(invalid)? as usize;
        let size = read32(bytes, at + 24).ok_or_else(invalid)? as usize;
        let name_length = read16(bytes, at + 28).ok_or_else(invalid)? as usize;
        let extra_length = read16(bytes, at + 30).ok_or_else(invalid)? as usize;
        let comment_length = read16(bytes, at + 32).ok_or_else(invalid)? as usize;
        let local = read32(bytes, at + 42).ok_or_else(invalid)? as usize;
        let name = bytes.get(at + 46..at + 46 + name_length).ok_or_else(invalid)?;
        let name = String::from_utf8_lossy(name).into_owned();
        at += 46 + name_length + extra_length + comment_length;

        if read32(bytes, local) != Some(0x0403_4b50) {
            return Err(invalid());
        }
        let local_name = read16(bytes, local + 26).ok_or_else(invalid)? as usize;
        let local_extra = read16(bytes, local + 28).ok_or_else(invalid)? as usize;
        let start = local + 30 + local_name + local_extra;
        let raw = bytes.get(start..start + compressed_size).ok_or_else(invalid)?;
        let data = match method {
            0 => raw.to_vec(),
            8 => {
                let mut data = Vec::with_capacity(size);
                DeflateDecoder::new(raw).read_to_end(&mut data).map_err(|error| format!("{name}: {error}"))?;
                data
            }
            other => return Err(format!("{name}: unsupported compression method {other}")),
        };
        if data.len() != size || crc32(&data) != checksum {
            return Err(format!("{name}: checksum mismatch"));
        }
        entries.push(ZipEntry { name, data });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archives_round_trip() {
        let entries = vec![
            ZipEntry::new("report.json", "{\n  \"a\": 1\n}\n"),
            ZipEntry::new("dir/ünïcode.md", vec![b'x'; 10_000]),
            ZipEntry::new("empty", Vec::new()),
        ];
        let archive = create_zip_archive(&entries).unwrap();
        assert_eq!(&archive[..4], b"PK\x03\x04");
        assert!(archive.len() < 10_000);
        assert_eq!(read_zip_archive(&archive).unwrap(), entries);
    }

    #[test]
    fn corrupt_archives_are_rejected() {
        let mut archive = create_zip_archive(&[ZipEntry::new("a", "hello")]).unwrap();
        assert_eq!(read_zip_archive(b"not a zip").unwrap_err(), "Invalid zip archive");
        let crc_at = 14;
        archive[crc_at] ^= 0xff;
        let directory = archive.len() - 22 - (46 + 1);
        archive[directory + 16] ^= 0xff;
        assert_eq!(read_zip_archive(&archive).unwrap_err(), "a: checksum mismatch");
    }

    #[test]
    fn dos_dates_use_utc_fields() {
        let (time, date) = dos_date_time(1_582_979_445);
        assert_eq!(time, (12 << 11) | (30 << 5) | (45 >> 1));
        assert_eq!(date, ((2020 - 1980) << 9) | (2 << 5) | 29);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }
}
