//! Conversion of a `state.redb` written by Seedwell 0.1 (redb 2.6, file format v2).
//!
//! redb 3 dropped the v2 file format and changed how variable-width tuple keys (`stats_daily`) are
//! encoded, so redb 4 refuses such a file with `UpgradeRequired`. [`convert`] reads every table
//! with redb 2.6 and writes the rows into a new file through redb 4, which re-encodes them. The
//! original is copied to `backups/` before anything else happens, and the new file replaces it
//! with an atomic rename, so a crash at any point leaves either the old file or the new one.
//!
//! The schema version is copied with the `meta` table, so the usual schema migrations run on the
//! converted file afterwards. This module can go once no 0.1 databases are left in the field.

use std::fs;
use std::path::{Path, PathBuf};

use redb::TableHandle;

use crate::error::StateError;
use crate::fsutil::{sync_dir, unique_stamped};
use crate::store::{
    COUNTERS, DHT, InfoHash, META, OpenFail, OpenOptions, PEERS, RESUME, ROOTS, STATS_DAILY, STATS_TOTALS, TIMING,
    TORRENTS, guarded, io_err,
};

/// A redb 2.6 failure: damage in the old file, or anything else.
fn old(e: impl Into<redb2::Error>) -> OpenFail {
    match e.into() {
        redb2::Error::Corrupted(m) => OpenFail::Corrupt(m),
        redb2::Error::Io(io)
            if matches!(io.kind(), std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof) =>
        {
            OpenFail::Corrupt(io.to_string())
        }
        other => OpenFail::Fatal(StateError::Legacy(other.to_string())),
    }
}

/// A redb 4 failure while writing the new file. Never a reason to set the old file aside.
fn new(e: impl Into<redb::Error>) -> OpenFail {
    OpenFail::Fatal(StateError::Legacy(e.into().to_string()))
}

/// Copy one table, if the old file has it. Key and value types are the same in both versions.
macro_rules! copy_table {
    ($rtx:expr, $w:expr, $def:expr, $k:ty, $v:ty) => {{
        let src_def: redb2::TableDefinition<$k, $v> = redb2::TableDefinition::new($def.name());
        match $rtx.open_table(src_def) {
            Ok(src) => {
                let mut dst = $w.open_table($def).map_err(new)?;
                for row in redb2::ReadableTable::iter(&src).map_err(old)? {
                    let (k, v) = row.map_err(old)?;
                    dst.insert(k.value(), v.value()).map_err(new)?;
                }
            }
            Err(redb2::TableError::TableDoesNotExist(_)) => {}
            Err(e) => return Err(old(e)),
        }
    }};
}

/// Convert the redb 2 file at `path` in place. Returns the backup of the original.
pub(crate) fn convert(dir: &Path, path: &Path, opts: &OpenOptions) -> Result<PathBuf, OpenFail> {
    let backups = dir.join("backups");
    fs::create_dir_all(&backups).map_err(io_err("create backups directory"))?;
    let backup = unique_stamped(&backups, "state.redb.redb2.", "");
    fs::copy(path, &backup).map_err(io_err("back up the database before converting it"))?;
    if let Ok(f) = fs::File::open(&backup) {
        let _ = f.sync_all();
    }

    let tmp = dir.join("state.redb.convert");
    let _ = fs::remove_file(&tmp);
    let copied = guarded(|| {
        let src = redb2::Builder::new().set_cache_size(opts.cache_bytes).open(path).map_err(old)?;
        let rtx = src.begin_read().map_err(old)?;
        let dst = redb::Builder::new().set_cache_size(opts.cache_bytes).create(&tmp).map_err(new)?;
        let w = dst.begin_write().map_err(new)?;
        copy_table!(rtx, w, META, &str, &[u8]);
        copy_table!(rtx, w, TORRENTS, InfoHash, &[u8]);
        copy_table!(rtx, w, RESUME, InfoHash, &[u8]);
        copy_table!(rtx, w, COUNTERS, InfoHash, &[u8]);
        copy_table!(rtx, w, TIMING, &[u8], &[u8]);
        copy_table!(rtx, w, STATS_DAILY, (&str, u32), &[u8]);
        copy_table!(rtx, w, STATS_TOTALS, &str, &[u8]);
        copy_table!(rtx, w, PEERS, InfoHash, &[u8]);
        copy_table!(rtx, w, DHT, &str, &[u8]);
        copy_table!(rtx, w, ROOTS, u32, &str);
        w.commit().map_err(new)?;
        Ok(())
    });
    if let Err(e) = copied {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    fs::File::open(&tmp).and_then(|f| f.sync_all()).map_err(io_err("sync the converted database"))?;
    fs::rename(&tmp, path).map_err(io_err("move the converted database into place"))?;
    sync_dir(dir).map_err(io_err("sync the data directory"))?;
    Ok(backup)
}
