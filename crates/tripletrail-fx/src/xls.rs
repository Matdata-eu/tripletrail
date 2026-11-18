//! A scan of a legacy `.xls` workbook before calamine reads it.
//!
//! calamine reads an `.xls` file whole when it opens it, and trusts what the file says:
//!
//! - it follows the compound file's sector chains without a step limit, so a chain that
//!   loops back reads forever (the directory chain is read to its end, growing without
//!   bound);
//! - it reserves a sheet's cells from the sheet's DIMENSIONS record, which a file of a few
//!   hundred bytes can set to billions;
//! - it makes each sheet a range as large as the box around its cells.
//!
//! This module walks the same structures with bounds, and refuses a file that would make
//! calamine allocate more than `max` cells, or whose chains loop or leave the file.

use crate::FxError;

const SIGNATURE: &[u8; 8] = b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1";
const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
/// Sector ids from here up are not sectors (DIFSECT, FATSECT, ENDOFCHAIN, FREESECT).
const DIFSECT: u32 = 0xFFFF_FFFC;
/// calamine reads a stream shorter than this from the mini stream.
const MINI_CUTOFF: usize = 4096;

/// Whether `bytes` is a compound file (a legacy `.xls` workbook, among others).
pub(crate) fn is_compound(bytes: &[u8]) -> bool {
    bytes.starts_with(SIGNATURE)
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

fn malformed(what: &str) -> FxError {
    FxError::Parse(format!("spreadsheet: the xls file is malformed ({what})"))
}

/// The sectors of a compound file, read as calamine reads them.
struct Compound<'a> {
    /// The file after its header.
    body: &'a [u8],
    size: usize,
    fats: Vec<u32>,
}

impl Compound<'_> {
    /// Sector `id`; a last sector may be short, as calamine reads it.
    fn sector(&self, id: u32) -> Result<&[u8], FxError> {
        let start = (id as usize)
            .checked_mul(self.size)
            .filter(|&s| s <= self.body.len())
            .ok_or_else(|| malformed("a sector lies beyond the end of the file"))?;
        Ok(&self.body[start..self.body.len().min(start + self.size)])
    }

    /// The chain from `start`, at most `len` bytes. A chain that loops or names a sector
    /// the FAT does not hold is refused: calamine would follow it forever or panic.
    fn chain(&self, start: u32, len: usize) -> Result<Vec<u8>, FxError> {
        let mut out = Vec::new();
        let mut id = start;
        let mut steps = 0usize;
        while id != ENDOFCHAIN {
            steps += 1;
            if steps > self.fats.len() + 1 {
                return Err(malformed("a sector chain loops"));
            }
            let sector = self.sector(id)?;
            out.extend_from_slice(&sector[..sector.len().min(len - out.len())]);
            if out.len() >= len {
                break;
            }
            id = *self
                .fats
                .get(id as usize)
                .ok_or_else(|| malformed("a sector chain leaves the allocation table"))?;
        }
        Ok(out)
    }
}

/// A mini stream chain, as calamine reads it: 64-byte sectors of `mini`, linked by
/// `mini_fats`.
fn mini_chain(mini: &[u8], mini_fats: &[u32], start: u32, len: usize) -> Result<Vec<u8>, FxError> {
    let mut out = Vec::new();
    let mut id = start;
    let mut steps = 0usize;
    while id != ENDOFCHAIN {
        steps += 1;
        if steps > mini_fats.len() + 1 {
            return Err(malformed("a mini sector chain loops"));
        }
        let begin = (id as usize).saturating_mul(64);
        if begin > mini.len() {
            return Err(malformed("a mini sector lies beyond the mini stream"));
        }
        let sector = &mini[begin..mini.len().min(begin + 64)];
        out.extend_from_slice(&sector[..sector.len().min(len - out.len())]);
        if out.len() >= len {
            break;
        }
        id = *mini_fats
            .get(id as usize)
            .ok_or_else(|| malformed("a mini sector chain leaves its table"))?;
    }
    Ok(out)
}

/// The workbook stream of a compound file, read with bounds.
fn workbook_stream(bytes: &[u8]) -> Result<Vec<u8>, FxError> {
    let header = bytes
        .get(..512)
        .ok_or_else(|| malformed("the header is short"))?;
    let size = match u16_at(header, 30) {
        Some(9) => 512,
        Some(12) => 4096,
        _ => return Err(malformed("the sector size is unknown")),
    };
    let large = size == 4096;
    let body = bytes.get(size..).unwrap_or_default();
    let read = |i| u32_at(header, i).unwrap_or(0);
    let (dir_start, mini_fat_start, mini_fat_len, difat_start) =
        (read(48), read(60), read(64) as usize, read(68));
    let mut compound = Compound {
        body,
        size,
        fats: Vec::new(),
    };
    let maxid = bytes.len() / size;

    // The DIFAT: 109 ids in the header, then a chain of DIFAT sectors.
    let mut difat: Vec<u32> = header[76..512]
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    let mut id = difat_start;
    let mut steps = 0usize;
    while id < DIFSECT {
        steps += 1;
        if steps > maxid + 1 {
            return Err(malformed("the DIFAT chain loops"));
        }
        let sector = compound.sector(id)?;
        difat.extend(
            sector
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes(c.try_into().unwrap())),
        );
        id = difat
            .pop()
            .ok_or_else(|| malformed("the DIFAT chain is short"))?;
    }
    let mut fats = Vec::new();
    for id in difat.into_iter().filter(|&id| id < DIFSECT) {
        fats.extend(
            compound
                .sector(id)?
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes(c.try_into().unwrap())),
        );
        if fats.len() > maxid.saturating_mul(size / 4) + size {
            return Err(malformed("the allocation table is larger than the file"));
        }
    }
    compound.fats = fats;

    let dirs = compound.chain(dir_start, usize::MAX)?;
    let entries: Vec<&[u8]> = dirs.chunks_exact(128).collect();
    let root = entries
        .first()
        .ok_or_else(|| malformed("there is no root directory"))?;
    let length = |e: &[u8]| -> usize {
        if large {
            u32_at(e, 120).unwrap_or(0) as usize | ((u32_at(e, 124).unwrap_or(0) as usize) << 32)
        } else {
            u32_at(e, 120).unwrap_or(0) as usize
        }
    };
    let name = |e: &[u8]| -> String {
        let units: Vec<u16> = e[..64]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        String::from_utf16_lossy(&units)
    };
    let (mini, mini_fats) = if mini_fat_len > 0 {
        let mini = compound.chain(u32_at(root, 116).unwrap_or(ENDOFCHAIN), length(root))?;
        let table = compound.chain(mini_fat_start, mini_fat_len.saturating_mul(size))?;
        let table: Vec<u32> = table
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        (mini, table)
    } else {
        (Vec::new(), Vec::new())
    };
    let entry = ["Workbook", "Book"]
        .iter()
        .find_map(|n| {
            entries
                .iter()
                .find(|e| name(e).as_bytes().eq_ignore_ascii_case(n.as_bytes()))
        })
        .ok_or_else(|| malformed("there is no workbook stream"))?;
    let start = u32_at(entry, 116).unwrap_or(ENDOFCHAIN);
    let len = length(entry);
    if len < MINI_CUTOFF {
        mini_chain(&mini, &mini_fats, start, len)
    } else {
        compound.chain(start, len)
    }
}

/// The records of a BIFF stream from `at`: (type, data). Continue records (0x003C) are
/// among them; calamine appends them to the record before, so they are no record here.
fn records(stream: &[u8], mut at: usize) -> impl Iterator<Item = (u16, &[u8])> {
    std::iter::from_fn(move || {
        let typ = u16_at(stream, at)?;
        let len = u16_at(stream, at + 2)? as usize;
        let data = stream.get(at + 4..at + 4 + len)?;
        at += 4 + len;
        Some((typ, data))
    })
}

/// A box around cell positions.
#[derive(Default)]
struct Bounds(Option<(u32, u32, u32, u32)>);

impl Bounds {
    fn add(&mut self, row: u32, first: u32, last: u32) {
        self.0 = Some(match self.0 {
            None => (row, row, first, last),
            Some((r0, r1, c0, c1)) => (r0.min(row), r1.max(row), c0.min(first), c1.max(last)),
        });
    }

    fn area(&self) -> u64 {
        self.0.map_or(0, |(r0, r1, c0, c1)| {
            u64::from(r1 - r0 + 1).saturating_mul(u64::from(c1 - c0 + 1))
        })
    }
}

/// Upper bounds of the bytes of the shared strings, as calamine decodes them: an SST record
/// and its Continue records, `segments`. A string's characters may run on into the next
/// Continue record, which starts with a byte of flags; a character becomes at most 3 bytes
/// of UTF-8.
fn shared_strings(segments: &[&[u8]]) -> Vec<u64> {
    let mut strings = Vec::new();
    let Some(first) = segments.first().and_then(|s| s.get(8..)) else {
        return strings;
    };
    let mut segs = std::iter::once(first).chain(segments[1..].iter().copied());
    let mut data: &[u8] = segs.next().unwrap_or_default();
    // Skips `n` bytes, across Continue records.
    fn skip<'a>(
        data: &mut &'a [u8],
        segs: &mut impl Iterator<Item = &'a [u8]>,
        mut n: usize,
    ) -> bool {
        loop {
            let k = n.min(data.len());
            *data = &data[k..];
            n -= k;
            if n == 0 {
                return true;
            }
            match segs.next() {
                Some(next) => *data = next,
                None => return false,
            }
        }
    }
    loop {
        if data.is_empty() {
            match segs.next() {
                Some(next) => data = next,
                None => return strings,
            }
            continue;
        }
        let (Some(cch), Some(&flags)) = (u16_at(data, 0), data.get(2)) else {
            return strings;
        };
        data = &data[3..];
        let mut high = flags & 1 != 0;
        let mut runs = 0usize;
        let mut ext = 0usize;
        if flags & 8 != 0 {
            let Some(n) = u16_at(data, 0) else {
                return strings;
            };
            runs = usize::from(n);
            data = &data[2..];
        }
        if flags & 4 != 0 {
            let Some(n) = u32_at(data, 0) else {
                return strings;
            };
            // calamine reads it as an i32 and casts it to usize.
            ext = n as i32 as usize;
            data = &data[4..];
        }
        let mut left = usize::from(cch);
        while left > 0 {
            let width = if high { 2 } else { 1 };
            let k = left.min(data.len() / width);
            data = &data[k * width..];
            left -= k;
            if left > 0 {
                match segs.next() {
                    Some(next) if !next.is_empty() => {
                        high = next[0] & 1 != 0;
                        data = &next[1..];
                    }
                    _ => {
                        // calamine fails on a string cut short; count it whole.
                        strings.push(u64::from(cch) * 3);
                        return strings;
                    }
                }
            }
        }
        strings.push(u64::from(cch) * 3);
        if !skip(&mut data, &mut segs, runs.saturating_mul(4)) || !skip(&mut data, &mut segs, ext) {
            return strings;
        }
    }
}

/// Refuses an `.xls` workbook that calamine would read into more than `max` cells: the
/// cells it reserves from each sheet's DIMENSIONS record, the cells and formulas it reads,
/// and the ranges around them.
pub(crate) fn check(bytes: &[u8], max: u64, max_bytes: u64) -> Result<(), FxError> {
    let stream = workbook_stream(bytes)?;
    // The sheets' offsets, from the BoundSheet records of the globals substream.
    let mut sheets = Vec::new();
    // The shared string table: its record and the Continue records after it.
    let mut sst: Vec<&[u8]> = Vec::new();
    let mut in_sst = false;
    for (typ, data) in records(&stream, 0) {
        if in_sst && typ == 0x003C {
            sst.push(data);
            continue;
        }
        in_sst = false;
        match typ {
            0x0085 => sheets.extend(u32_at(data, 0)),
            0x00FC => {
                (sst, in_sst) = (vec![data], true);
            }
            0x000A => break,
            _ => {}
        }
    }
    // calamine copies a shared string into each cell that names it (LabelSst).
    let strings = shared_strings(&sst);
    let mut text: u64 = strings.iter().sum();
    let too_much_text = |n: u64| {
        FxError::Unsupported(format!(
            "spreadsheet: the workbook's cells name its shared strings for more than \
             {max_bytes} bytes of text ({n} or more; spreadsheet.max-bytes)"
        ))
    };
    if text > max_bytes {
        return Err(too_much_text(text));
    }
    let mut total: u64 = 0;
    let refuse = |n: u64| {
        FxError::Unsupported(format!(
            "spreadsheet: the workbook asks for more than {max} cells ({n} or more, counting \
             the cells its sheets reserve and the ranges around them; spreadsheet.max-cells)"
        ))
    };
    // calamine reads each sheet from its offset to its end. Sheets do not overlap in a
    // workbook; many sheets naming the same bytes would read them over and over. In order
    // of their offsets, a sheet must end before the next begins.
    sheets.sort_unstable();
    for (i, &pos) in sheets.iter().enumerate() {
        let pos = pos as usize;
        let limit = sheets
            .get(i + 1)
            .map_or(stream.len(), |&next| next as usize);
        let mut end = pos;
        if pos > stream.len() {
            return Err(malformed("a sheet starts beyond the workbook stream"));
        }
        let (mut values, mut formulas) = (Bounds::default(), Bounds::default());
        let (mut reserved, mut read) = (0u64, 0u64);
        let mut formula_at = (0u32, 0u32);
        let (mut seen_bof, mut depth) = (false, 0usize);
        for (typ, data) in records(&stream, pos) {
            end += 4 + data.len();
            if end > limit {
                return Err(malformed("its sheets overlap"));
            }
            if typ == 0x003C {
                continue;
            }
            if depth > 0 {
                match typ {
                    0x0809 => depth += 1,
                    0x000A => depth -= 1,
                    _ => {}
                }
                continue;
            }
            let at = |d: &[u8]| (u16_at(d, 0).map(u32::from), u16_at(d, 2).map(u32::from));
            match typ {
                0x0809 if seen_bof => depth += 1,
                0x0809 => seen_bof = true,
                0x000A => break,
                // DIMENSIONS: calamine reserves rows x columns cells, computing them as
                // it does (a first row past the last wraps around to billions).
                0x0200 => {
                    let (rf, rl, mut cf, cl) = match data.len() {
                        10 => (
                            u32::from(u16_at(data, 0).unwrap_or(0)),
                            u32::from(u16_at(data, 2).unwrap_or(0)),
                            u32::from(u16_at(data, 4).unwrap_or(0)),
                            u32::from(u16_at(data, 6).unwrap_or(0)),
                        ),
                        14 | 16 => (
                            u32_at(data, 0).unwrap_or(0),
                            u32_at(data, 4).unwrap_or(0),
                            u32::from(u16_at(data, 8).unwrap_or(0)),
                            u32::from(u16_at(data, 10).unwrap_or(0)),
                        ),
                        _ => (0, 1, 0, 1),
                    };
                    if cf > 0xFF || cl < cf {
                        cf = 0;
                    }
                    let (rows, cols) = if rl >= 1 && cl >= 1 {
                        (
                            (rl - 1).wrapping_sub(rf).wrapping_add(1),
                            (cl - 1).wrapping_sub(cf).wrapping_add(1),
                        )
                    } else {
                        (1, 1)
                    };
                    reserved =
                        reserved.saturating_add(u64::from(rows).saturating_mul(u64::from(cols)));
                }
                // Number, Label, RString, BoolErr, Rk, LabelSst.
                0x0203 | 0x0204 | 0x00D6 | 0x0205 | 0x027E | 0x00FD => {
                    if let (Some(r), Some(c)) = at(data) {
                        values.add(r, c, c);
                        read += 1;
                    }
                    if typ == 0x00FD
                        && let Some(&n) = u32_at(data, 6).and_then(|i| strings.get(i as usize))
                    {
                        text = text.saturating_add(n);
                        if text > max_bytes {
                            return Err(too_much_text(text));
                        }
                    }
                }
                // String, the text value of the formula before.
                0x0207 => {
                    values.add(formula_at.0, formula_at.1, formula_at.1);
                    read += 1;
                }
                // MulRk: a run of cells in one row. calamine computes the run's length
                // as a u16 `last - first + 1`, which wraps when the last column is below
                // the first, accepts the record when its length matches that count, and
                // then numbers the cells from the first column on as u32, past 65535.
                0x00BD => {
                    if let (Some(r), Some(c), Some(last)) = (
                        u16_at(data, 0),
                        u16_at(data, 2),
                        u16_at(data, data.len().max(2) - 2),
                    ) {
                        let count = last.wrapping_sub(c).wrapping_add(1) as usize;
                        if data.len() == 6 + 6 * count && count > 0 {
                            let first = u32::from(c);
                            values.add(u32::from(r), first, first + count as u32 - 1);
                            read += count as u64;
                        }
                    }
                }
                0x0006 => {
                    if let (Some(r), Some(c)) = at(data) {
                        formula_at = (r, c);
                        values.add(r, c, c);
                        formulas.add(r, c, c);
                        read += 2;
                    }
                }
                _ => {}
            }
            let n = total
                .saturating_add(reserved)
                .saturating_add(read)
                .saturating_add(values.area())
                .saturating_add(formulas.area());
            if n > max {
                return Err(refuse(n));
            }
        }
        total = total
            .saturating_add(reserved)
            .saturating_add(read)
            .saturating_add(values.area())
            .saturating_add(formulas.area());
    }
    Ok(())
}
