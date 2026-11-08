//! Unpacks an uploaded configuration archive (`.tar.gz`) into the single configuration text a run
//! uses. Nothing is written to disk: entries are read in memory, never extracted by path.
//!
//! What is supported: `.tf` files at the root of the archive, joined in name order. What is refused,
//! with a message that says why: `.tf.json` files (they cannot be joined into HCL), `.tf` files in
//! subdirectories (local modules are not supported yet), links, and anything that unpacks to more
//! than the configuration limit.

use flate2::read::GzDecoder;
use std::io::Read;
use tar::EntryType;

/// Most the archive may unpack to, summed over every entry, before it is refused.
const MAX_UNPACKED: u64 = 20_000_000;

pub fn extract_config(gz: &[u8], max_config: usize) -> Result<String, String> {
    if gz.is_empty() {
        return Err("the upload is empty".into());
    }
    // Reads the gzip stream through a cap, so a small file that expands enormously stops early.
    let capped = GzDecoder::new(gz).take(MAX_UNPACKED + 1);
    let mut ar = tar::Archive::new(capped);
    let entries = ar.entries().map_err(|_| "the upload is not a .tar.gz archive".to_string())?;
    let mut files: Vec<(String, String)> = Vec::new();
    let (mut unpacked, mut config_bytes) = (0u64, 0usize);
    for entry in entries {
        let mut entry = entry.map_err(|_| "the upload is not a valid .tar.gz archive".to_string())?;
        let kind = entry.header().entry_type();
        let raw_path = entry.path().map_err(|_| "an entry has an unreadable path".to_string())?.to_string_lossy().replace('\\', "/");
        let path = raw_path.trim_start_matches("./").to_string();
        if path.is_empty() || path == "." {
            continue;
        }
        if path.starts_with('/') || path.split('/').any(|c| c == "..") {
            return Err(format!("the archive has an unsafe path: {raw_path}"));
        }
        match kind {
            EntryType::Directory | EntryType::XGlobalHeader | EntryType::XHeader => continue,
            EntryType::Regular | EntryType::Continuous => {}
            _ => return Err(format!("the archive has a link or special file, which is not allowed: {raw_path}")),
        }
        let size = entry.header().size().unwrap_or(0);
        unpacked += size;
        if unpacked > MAX_UNPACKED {
            return Err("the archive unpacks to more than 20 MB".into());
        }
        let is_tf = path.ends_with(".tf");
        let nested = path.contains('/');
        if path.ends_with(".tf.json") {
            return Err(format!("{path}: .tf.json files are not supported yet; use .tf files"));
        }
        if nested {
            if is_tf {
                return Err(format!("{path}: modules in subdirectories are not supported yet; keep .tf files at the top of the archive"));
            }
            continue;
        }
        if !is_tf {
            continue; // README, lock file and the like are not configuration
        }
        config_bytes += size as usize;
        if config_bytes > max_config {
            return Err(format!("the configuration is limited to {} MB", max_config / 1_000_000));
        }
        let mut text = String::new();
        entry.read_to_string(&mut text).map_err(|_| format!("{path} is not valid UTF-8 text"))?;
        files.push((path, text));
    }
    if files.is_empty() {
        return Err("the archive has no .tf files at its top level".into());
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::new();
    for (name, text) in files {
        out.push_str(&format!("# file: {name}\n{text}"));
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    Ok(out)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use flate2::{write::GzEncoder, Compression};
    use std::io::Write;

    /// A .tar.gz of (path, content) pairs.
    pub fn make(files: &[(&str, &str)]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (path, content) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(content.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, path, content.as_bytes()).unwrap();
        }
        let raw = tar.into_inner().unwrap();
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&raw).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn root_tf_files_are_joined_in_name_order() {
        let a = make(&[("b.tf", "resource \"b\" \"y\" {}"), ("./a.tf", "resource \"a\" \"x\" {}\n"), ("README.md", "ignored"), ("docs/notes.txt", "ignored")]);
        let text = extract_config(&a, 1_000_000).unwrap();
        assert_eq!(text, "# file: a.tf\nresource \"a\" \"x\" {}\n# file: b.tf\nresource \"b\" \"y\" {}\n");
    }

    #[test]
    fn what_cannot_be_used_is_refused_with_a_reason() {
        let err = |files: &[(&str, &str)]| extract_config(&make(files), 1_000_000).unwrap_err();
        assert!(err(&[("modules/net/main.tf", "x")]).contains("subdirectories"));
        assert!(err(&[("main.tf.json", "{}")]).contains(".tf.json"));
        assert!(err(&[("notes.md", "x")]).contains("no .tf files"));
        assert!(unsafe_path_error("../escape.tf").contains("unsafe"));
        assert!(unsafe_path_error("/abs/main.tf").contains("unsafe"));
        assert!(extract_config(&make(&[("a.tf", &"x".repeat(100))]), 50).unwrap_err().contains("limited"));
        assert!(extract_config(b"not an archive", 1000).unwrap_err().contains("archive"));
        assert!(extract_config(b"", 1000).unwrap_err().contains("empty"));
    }

    /// The builder refuses `..` itself, so the name is written straight into the header.
    fn unsafe_path_error(name: &str) -> String {
        let mut h = tar::Header::new_gnu();
        h.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name.as_bytes());
        h.set_size(1);
        h.set_mode(0o644);
        h.set_cksum();
        let mut tar = tar::Builder::new(Vec::new());
        tar.append(&h, &b"x"[..]).unwrap();
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&tar.into_inner().unwrap()).unwrap();
        extract_config(&gz.finish().unwrap(), 1000).unwrap_err()
    }

    #[test]
    fn links_are_refused() {
        let mut tar = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(EntryType::Symlink);
        h.set_size(0);
        tar.append_link(&mut h, "link.tf", "/etc/passwd").unwrap();
        let raw = tar.into_inner().unwrap();
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&raw).unwrap();
        let e = extract_config(&gz.finish().unwrap(), 1000).unwrap_err();
        assert!(e.contains("link"), "{e}");
    }

    #[test]
    fn an_archive_that_expands_enormously_is_stopped() {
        // 25 MB of zeros compresses to a few KB; the cap must stop it, and with a clear reason.
        let big = "0".repeat(25_000_000);
        let e = extract_config(&make(&[("a.tf", &big)]), 100_000_000).unwrap_err();
        assert!(e.contains("20 MB") || e.contains("archive"), "{e}");
    }
}
