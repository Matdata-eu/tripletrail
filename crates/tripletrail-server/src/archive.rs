//! Compressed uploads for the RDF4J API (#223): RDF4J's client sends a gzip or zip file to the
//! server as it is, and the server unpacks it. A zip's entries are read from its central
//! directory, stored or deflated; ZIP64 and encrypted entries are refused.

use std::io::Read;

/// Unpacked documents: an entry name (for zip files) and the bytes.
pub type Documents = Vec<(Option<String>, Vec<u8>)>;

/// The documents in a body: itself, or what a gzip or zip file holds, with entry names.
/// `limit` bounds the unpacked size.
pub fn unpack(body: Vec<u8>, limit: Option<usize>) -> Result<Documents, String> {
    if body.starts_with(&[0x1f, 0x8b]) {
        let out = read_limited(flate2::read::MultiGzDecoder::new(&body[..]), limit)?;
        return Ok(vec![(None, out)]);
    }
    if body.starts_with(b"PK\x03\x04") {
        return unzip(&body, limit);
    }
    Ok(vec![(None, body)])
}

fn read_limited(reader: impl Read, limit: Option<usize>) -> Result<Vec<u8>, String> {
    let max = limit.map_or(u64::MAX, |l| l as u64 + 1);
    let mut out = Vec::new();
    reader
        .take(max)
        .read_to_end(&mut out)
        .map_err(|e| format!("Cannot unpack the upload: {e}"))?;
    if limit.is_some_and(|l| out.len() > l) {
        return Err("The unpacked upload is larger than the upload limit".to_owned());
    }
    Ok(out)
}

fn u16_at(d: &[u8], at: usize) -> Result<usize, String> {
    d.get(at..at + 2)
        .map(|b| usize::from(u16::from_le_bytes([b[0], b[1]])))
        .ok_or_else(|| "Truncated zip file".to_owned())
}

fn u32_at(d: &[u8], at: usize) -> Result<usize, String> {
    d.get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
        .ok_or_else(|| "Truncated zip file".to_owned())
}

fn unzip(d: &[u8], limit: Option<usize>) -> Result<Documents, String> {
    // The end of central directory record is in the last 64 KiB + 22 bytes.
    let from = d.len().saturating_sub(65_557);
    let end = (from..d.len().saturating_sub(21))
        .rev()
        .find(|&i| d[i..].starts_with(b"PK\x05\x06"))
        .ok_or("A zip file without a central directory")?;
    let entries = u16_at(d, end + 10)?;
    let mut at = u32_at(d, end + 16)?;
    if entries == 0xffff || at == 0xffff_ffff {
        return Err("ZIP64 files are not supported".to_owned());
    }
    let mut out = Vec::new();
    let mut total = 0usize;
    for _ in 0..entries {
        if !d.get(at..).is_some_and(|r| r.starts_with(b"PK\x01\x02")) {
            return Err("A damaged zip central directory".to_owned());
        }
        let flags = u16_at(d, at + 8)?;
        let method = u16_at(d, at + 10)?;
        let compressed = u32_at(d, at + 20)?;
        let name_len = u16_at(d, at + 28)?;
        let extra_len = u16_at(d, at + 30)?;
        let comment_len = u16_at(d, at + 32)?;
        let local = u32_at(d, at + 42)?;
        let name = d
            .get(at + 46..at + 46 + name_len)
            .ok_or("Truncated zip file")?;
        let name = String::from_utf8_lossy(name).into_owned();
        at += 46 + name_len + extra_len + comment_len;
        if name.ends_with('/') {
            continue;
        }
        if flags & 1 != 0 {
            return Err(format!("{name}: encrypted zip entries are not supported"));
        }
        if !d.get(local..).is_some_and(|r| r.starts_with(b"PK\x03\x04")) {
            return Err(format!("{name}: a damaged zip entry"));
        }
        let start = local + 30 + u16_at(d, local + 26)? + u16_at(d, local + 28)?;
        let data = d
            .get(start..start.saturating_add(compressed))
            .ok_or("Truncated zip file")?;
        let left = limit.map(|l| l.saturating_sub(total));
        let content = match method {
            0 => read_limited(data, left)?,
            8 => read_limited(flate2::read::DeflateDecoder::new(data), left)?,
            m => {
                return Err(format!(
                    "{name}: zip compression method {m} is not supported"
                ));
            }
        };
        total += content.len();
        out.push((Some(name), content));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn gzip_and_zip_unpack() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(b"<urn:a> <urn:b> <urn:c> .").unwrap();
        let docs = unpack(gz.finish().unwrap(), None).unwrap();
        assert_eq!(docs, [(None, b"<urn:a> <urn:b> <urn:c> .".to_vec())]);
        assert!(
            unpack(
                {
                    let mut gz =
                        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                    gz.write_all(&[b'x'; 100]).unwrap();
                    gz.finish().unwrap()
                },
                Some(10)
            )
            .is_err()
        );

        // A zip with one stored and one deflated entry.
        let mut deflated =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
        deflated.write_all(b"two").unwrap();
        let entries: [(&str, u16, Vec<u8>); 2] = [
            ("a.ttl", 0, b"one".to_vec()),
            ("b.nt", 8, deflated.finish().unwrap()),
        ];
        let mut zip = Vec::new();
        let mut central = Vec::new();
        for (name, method, data) in &entries {
            let offset = zip.len() as u32;
            zip.extend_from_slice(b"PK\x03\x04");
            zip.extend_from_slice(&[20, 0, 0, 0]);
            zip.extend_from_slice(&method.to_le_bytes());
            zip.extend_from_slice(&[0; 8]); // time, date, crc
            zip.extend_from_slice(&(data.len() as u32).to_le_bytes());
            zip.extend_from_slice(&[0; 4]);
            zip.extend_from_slice(&(name.len() as u16).to_le_bytes());
            zip.extend_from_slice(&[0, 0]);
            zip.extend_from_slice(name.as_bytes());
            zip.extend_from_slice(data);
            central.extend_from_slice(b"PK\x01\x02");
            central.extend_from_slice(&[20, 0, 20, 0, 0, 0]);
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&[0; 8]);
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&[0; 4]);
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0; 12]);
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_offset = zip.len() as u32;
        zip.extend_from_slice(&central);
        zip.extend_from_slice(b"PK\x05\x06\0\0\0\0\x02\0\x02\0");
        zip.extend_from_slice(&(central.len() as u32).to_le_bytes());
        zip.extend_from_slice(&cd_offset.to_le_bytes());
        zip.extend_from_slice(&[0, 0]);
        let docs = unpack(zip, None).unwrap();
        assert_eq!(
            docs,
            [
                (Some("a.ttl".to_owned()), b"one".to_vec()),
                (Some("b.nt".to_owned()), b"two".to_vec())
            ]
        );
        assert_eq!(
            unpack(b"plain".to_vec(), None).unwrap(),
            [(None, b"plain".to_vec())]
        );
    }
}
