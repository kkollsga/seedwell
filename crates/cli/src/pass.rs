//! Files agents pass to each other through a synced folder (SPEC §7G.10): too big for a mail, and not meant to stay.
//!
//! The sender copies them into `Seedwell Pass-Through/<to>/<when> from <sender>/` in a synced folder both computers
//! have (the one that goes with the mail channel, or `--via`), and the mail says where, with each file's sha256. The
//! recipient's `mail receive` checks them, copies them out and deletes the copy: the delete syncs, so it is gone on
//! both computers, and a delete there is never archived. What is left over (a pass never received) is easy to find
//! and to clean up: `mail passes`.

use std::path::{Path, PathBuf};

use trs_control::proto::*;
use trs_control::types::*;

use crate::commands::{CliError, R, ask};

/// The pass-through folder at the root of a synced folder (the same name as `trs_sync::PASS_THROUGH`, whose deletes
/// are never archived).
pub const DIR: &str = "Seedwell Pass-Through";

/// What the pass-through folder's README says.
const README: &str = "Files that agents pass to each other through Seedwell (seedwell mail send --pass).\n\nEach folder below is one pass: <to project>/<when> from <sender>/. The recipient deletes it once it has the files\n(seedwell mail receive), which removes it on every computer. What is still here after a few days was never received:\nit is safe to delete (seedwell mail passes --clean). The originals stay with their senders.\n";

/// One passed file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Passed {
    /// Its file name (the pass's folder holds it).
    pub name: String,
    /// Its sha256, hex.
    pub sha256: String,
    /// Its size.
    pub bytes: u64,
}

/// A pass: the synced folder (its id), the pass's folder in it (relative), and the files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pass {
    /// The synced folder, by id.
    pub folder: String,
    /// The pass's folder in it, relative (`Seedwell Pass-Through/<to>/<when> from <sender>`).
    pub rel: String,
    /// What it holds.
    pub files: Vec<Passed>,
}

impl Pass {
    /// The mail header's `pass:` value.
    pub fn header(&self) -> String {
        format!("{}:{}", self.folder, self.rel)
    }

    /// The mail's section about it: where the files are and how to take them, then their sums (`sha256 bytes name`).
    pub fn section(&self, folder_name: &str) -> String {
        let sums: Vec<String> = self.files.iter().map(|f| format!("{} {} {}", f.sha256, f.bytes, f.name)).collect();
        format!(
            "**Passed files** ({}, {}) in the synced folder *{folder_name}*, `{}/`. Take them with `seedwell mail receive <this mail> --to DIR`: that checks them and deletes them from the pass-through folder, on both computers.\n\n```pass\n{}\n```",
            self.files.len(),
            size(self.files.iter().map(|f| f.bytes).sum()),
            self.rel,
            sums.join("\n")
        )
    }

    /// The pass in a mail: its header's `pass` value and the sums block of its text.
    pub fn of(header: &str, text: &str) -> Option<Pass> {
        let (folder, rel) = header.split_once(':')?;
        let block = text.split("```pass\n").nth(1)?.split("\n```").next()?;
        let files = block
            .lines()
            .filter_map(|l| {
                let mut w = l.splitn(3, ' ');
                let (sha256, bytes, name) = (w.next()?, w.next()?.parse().ok()?, w.next()?);
                Some(Passed { name: name.to_string(), sha256: sha256.to_string(), bytes })
            })
            .collect::<Vec<_>>();
        (!files.is_empty()).then(|| Pass { folder: folder.to_string(), rel: rel.to_string(), files })
    }
}

/// "1.2 MB".
pub fn size(b: u64) -> String {
    match b {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{b} bytes"),
    }
}

/// The synced folders on this Mac, this Mac's device id and its name.
pub async fn folders(c: &mut trs_control::client::Client) -> R<(Vec<SyncFolderDto>, String, String)> {
    let (_, l): (_, SyncListDto) = ask(c, Request::SyncList(Empty {})).await?;
    Ok((l.folders, l.device_id, l.device_name))
}

/// A device id's first group (`AJA5NYA`), as the markers name it.
fn group(device: &str) -> &str {
    device.split('-').next().unwrap_or(device)
}

/// One device's status in a pass folder (its `.status-<device>`, written by that device alone): `sent` (the sender),
/// `receiving` or `arrived`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// The device's name.
    pub computer: String,
    /// `sent`, `receiving` or `arrived`.
    pub state: String,
    /// Since when, Unix seconds.
    pub at: u64,
}

/// Every device's status in the pass folder `dir`, oldest first.
pub fn statuses(dir: &Path) -> Vec<Status> {
    let mut out: Vec<Status> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".status-"))
        .filter_map(|e| {
            let t = std::fs::read_to_string(e.path()).ok()?;
            let field = |k: &str| t.lines().find_map(|l| l.strip_prefix(k)).map(|v| v.trim().to_string());
            Some(Status { computer: field("computer: ")?, state: field("state: ")?, at: field("at: ")?.parse().ok()? })
        })
        .collect();
    // Within one second (times are whole seconds) the order of what happens: sent, then receiving, then arrived; the
    // folder lists its files in no order of its own.
    let stage = |s: &str| match s {
        "sent" => 0,
        "receiving" => 1,
        _ => 2,
    };
    out.sort_by(|a, b| (a.at, stage(&a.state), &a.computer).cmp(&(b.at, stage(&b.state), &b.computer)));
    out
}

/// The other devices' statuses in words: "Laptop: arrived 09:28 · iMac: receiving since 09:27".
pub fn words(list: &[Status]) -> String {
    list.iter()
        .filter(|s| s.state != "sent")
        .map(|s| match s.state.as_str() {
            "receiving" => format!("{}: receiving since {}", s.computer, crate::chat::time_s(s.at)),
            st => format!("{}: {st} {}", s.computer, crate::chat::time_s(s.at)),
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The folder a pass goes through: `via` (its id, the start of its id, or its name), else the one that goes with the
/// mail channel; it must be writable here.
pub fn pick<'a>(all: &'a [SyncFolderDto], channel_id: &str, channel_name: &str, via: Option<&str>) -> Result<&'a SyncFolderDto, CliError> {
    let f = match via {
        Some(v) => {
            let v = v.trim();
            all.iter().find(|f| f.id == v || f.name.eq_ignore_ascii_case(v)).or_else(|| all.iter().find(|f| v.len() >= 4 && f.id.starts_with(v))).ok_or_else(|| {
                CliError::plain(3, format!(concat!("no synced folder {v} on this ", trs_platform::computer!(), " (seedwell sync list: {})"), all.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(", "), v = v))
            })?
        }
        None => all.iter().find(|f| f.channel_id.as_deref() == Some(channel_id)).ok_or_else(|| {
            CliError::plain(
                2,
                format!(
                    "no synced folder goes with {channel_name}: pass the files through one that both computers have, with --via FOLDER (seedwell sync list: {})",
                    if all.is_empty() { "none yet".to_string() } else { all.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(", ") }
                ),
            )
        })?,
    };
    if f.access == "read_only" || f.access == "read" {
        return Err(CliError::plain(2, format!(concat!("this ", trs_platform::computer!(), " may only read {}: pass the files through a folder it may write (--via FOLDER)"), f.name)));
    }
    Ok(f)
}

fn err(what: &str, p: &Path, e: std::io::Error) -> CliError {
    CliError::plain(1, format!("{what} {}: {e}", p.display()))
}

fn sha256_of(p: &Path) -> std::io::Result<String> {
    use sha2::Digest;
    use std::io::Read;
    let mut f = std::fs::File::open(p)?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Copies `files` into the folder's pass-through folder for `to` (a folder of their own: `<when> from <me>`). Each is
/// copied into the folder's state directory first and moved in whole, so the folder never syncs half a file.
pub fn put(folder: &SyncFolderDto, device: &str, computer: &str, to: &str, me: &str, files: &[String]) -> R<Pass> {
    let root = PathBuf::from(&folder.path);
    let mut passed = Vec::new();
    let mut sources = Vec::new();
    for f in files {
        let p = PathBuf::from(f);
        let meta = std::fs::metadata(&p).map_err(|e| err("cannot pass", &p, e))?;
        if !meta.is_file() {
            return Err(CliError::plain(2, format!("{f} is not a file: pass files (a folder: zip it first)")));
        }
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| f.clone());
        if passed.iter().any(|x: &Passed| x.name == name) {
            return Err(CliError::plain(2, format!("two files are called {name}; pass them in two mails or rename one")));
        }
        passed.push(Passed { name, sha256: sha256_of(&p).map_err(|e| err("cannot read", &p, e))?, bytes: meta.len() });
        sources.push(p);
    }
    let base = root.join(DIR);
    std::fs::create_dir_all(&base).map_err(|e| err("cannot make", &base, e))?;
    let readme = base.join("README.txt");
    if !readme.exists() {
        let _ = std::fs::write(&readme, README);
    }
    let when = crate::chat::time_s(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())).replace(':', ".");
    let clean = |s: &str| s.replace(['/', ':'], "-");
    let mut rel = format!("{DIR}/{}/{when} from {}", clean(to), clean(me));
    let mut n = 2;
    while root.join(&rel).exists() {
        rel = format!("{DIR}/{}/{when} from {} {n}", clean(to), clean(me));
        n += 1;
    }
    let staging = root.join(".seedwell").join(format!("pass-{}", std::process::id()));
    std::fs::create_dir_all(&staging).map_err(|e| err("cannot make", &staging, e))?;
    for (src, f) in sources.iter().zip(&passed) {
        // A copy on the same disk is a clone (APFS): it costs no space until one of them changes.
        std::fs::copy(src, staging.join(&f.name)).map_err(|e| err("cannot copy", src, e))?;
    }
    // This Mac's status, first to arrive anywhere (the smallest file): the others learn a pass is coming, and this
    // Mac's Seedwell never reports it as receiving its own pass.
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let _ = std::fs::write(staging.join(format!(".status-{}", group(device))), format!("computer: {computer}\nstate: sent\nat: {now}\nfrom: {me}\n"));
    let dest = root.join(&rel);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| err("cannot make", parent, e))?;
    }
    std::fs::rename(&staging, &dest).map_err(|e| err("cannot move into", &dest, e))?;
    Ok(Pass { folder: folder.id.clone(), rel, files: passed })
}

/// What `receive` found.
pub enum Got {
    /// Every file is there and checked: copied to these paths, and the pass removed.
    Received(Vec<PathBuf>),
    /// Not all there yet (still syncing): which are missing or incomplete.
    Waiting(Vec<String>),
}

/// Takes a pass: when every file is there with its sha256, copies them into `to` (refusing to replace a different
/// file unless `force`) and deletes the pass's folder (and its project's, when that is empty).
pub fn receive(root: &Path, pass: &Pass, to: &Path, force: bool) -> R<Got> {
    let dir = root.join(&pass.rel);
    let mut missing = Vec::new();
    for f in &pass.files {
        let p = dir.join(&f.name);
        let ok = std::fs::metadata(&p).is_ok_and(|m| m.len() == f.bytes) && sha256_of(&p).is_ok_and(|h| h == f.sha256);
        if !ok {
            missing.push(f.name.clone());
        }
    }
    if !missing.is_empty() {
        return Ok(Got::Waiting(missing));
    }
    std::fs::create_dir_all(to).map_err(|e| err("cannot make", to, e))?;
    let mut out = Vec::new();
    for f in &pass.files {
        let target = to.join(&f.name);
        if target.exists() && !force && !sha256_of(&target).is_ok_and(|h| h == f.sha256) {
            return Err(CliError::plain(2, format!("{} exists and is a different file; give another --to, or --force to replace it", target.display())));
        }
        std::fs::copy(dir.join(&f.name), &target).map_err(|e| err("cannot copy to", &target, e))?;
        out.push(target);
    }
    std::fs::remove_dir_all(&dir).map_err(|e| err("cannot remove", &dir, e))?;
    if let Some(project) = dir.parent() {
        let _ = std::fs::remove_dir(project); // only when empty
    }
    Ok(Got::Received(out))
}

/// A pass found in a pass-through folder (`mail passes`).
pub struct Left {
    /// The synced folder's name.
    pub folder: String,
    /// Where the pass is.
    pub path: PathBuf,
    /// `<to>/<when> from <sender>`.
    pub name: String,
    /// Its files' size.
    pub bytes: u64,
    /// How old, in seconds.
    pub age_s: u64,
    /// Each device's status in it.
    pub statuses: Vec<Status>,
}

/// Every pass still in the pass-through folders of `folders`, oldest first.
pub fn left(folders: &[SyncFolderDto]) -> Vec<Left> {
    let now = std::time::SystemTime::now();
    let mut out = Vec::new();
    for f in folders {
        let base = PathBuf::from(&f.path).join(DIR);
        for to in std::fs::read_dir(&base).into_iter().flatten().flatten().filter(|e| e.path().is_dir()) {
            for p in std::fs::read_dir(to.path()).into_iter().flatten().flatten().filter(|e| e.path().is_dir()) {
                let path = p.path();
                let bytes = std::fs::read_dir(&path).into_iter().flatten().flatten().filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum();
                let age_s = p.metadata().and_then(|m| m.modified()).ok().and_then(|t| now.duration_since(t).ok()).map_or(0, |d| d.as_secs());
                let name = format!("{}/{}", to.file_name().to_string_lossy(), p.file_name().to_string_lossy());
                let statuses = statuses(&path);
                out.push(Left { folder: f.name.clone(), path, name, bytes, age_s, statuses });
            }
        }
    }
    out.sort_by_key(|l| std::cmp::Reverse(l.age_s));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_of_one_second_are_in_the_order_they_happen() {
        let dir = tempfile::tempdir().unwrap();
        // Written in an order the folder may list them in: the receiver's first.
        for (name, computer, state) in [("Z", "Laptop", "arrived"), ("Y", "iMac", "receiving"), ("A", "Studio Mac", "sent")] {
            std::fs::write(dir.path().join(format!(".status-{name}")), format!("computer: {computer}\nstate: {state}\nat: 1800000000\n")).unwrap();
        }
        let states: Vec<String> = statuses(dir.path()).into_iter().map(|s| s.state).collect();
        assert_eq!(states, ["sent", "receiving", "arrived"]);
    }

    #[test]
    fn a_pass_goes_in_whole_and_is_taken_out_whole() {
        let synced = tempfile::tempdir().unwrap();
        std::fs::create_dir(synced.path().join(".seedwell")).unwrap();
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("graph.bin"), vec![7u8; 300_000]).unwrap();
        std::fs::write(src.path().join("notes.md"), b"# notes").unwrap();
        let folder = SyncFolderDto { id: "LO7RVW3-X2SCCL3".into(), name: "Bridge".into(), path: synced.path().display().to_string(), access: "read_write".into(), ..Default::default() };
        let files = [src.path().join("graph.bin").display().to_string(), src.path().join("notes.md").display().to_string()];
        let pass = put(&folder, "AJA5NYA-X2SCCL3", "Studio Mac", "kglite", "mcp-servers", &files).unwrap();
        let dir = synced.path().join(&pass.rel);
        assert_eq!(statuses(&dir).iter().map(|s| (s.computer.as_str(), s.state.as_str())).collect::<Vec<_>>(), [("Studio Mac", "sent")], concat!("this ", trs_platform::computer!(), " sent it"));
        assert_eq!(words(&statuses(&dir)), "", "nobody else yet");
        std::fs::write(dir.join(".status-K2MSCFE"), "computer: Laptop\nstate: arrived\nat: 1800000000\n").unwrap();
        std::fs::write(dir.join(".status-QKZ4XNA"), "computer: iMac\nstate: receiving\nat: 1800000001\n").unwrap();
        let w = words(&statuses(&dir));
        assert!(w.starts_with("Laptop: arrived ") && w.contains(" · iMac: receiving since "), "{w}");
        assert!(pass.rel.starts_with("Seedwell Pass-Through/kglite/") && pass.rel.ends_with(" from mcp-servers"), "{}", pass.rel);
        assert!(synced.path().join("Seedwell Pass-Through/README.txt").exists());
        assert!(!std::fs::read_dir(synced.path().join(".seedwell")).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().starts_with("pass-")), "nothing staged is left");

        // The mail carries it; the recipient reads it back from the header and the text.
        let text = format!("Here they are.\n\n{}", pass.section("Bridge"));
        assert_eq!(Pass::of(&pass.header(), &text).as_ref(), Some(&pass));
        assert_eq!(left(std::slice::from_ref(&folder))[0].statuses.len(), 3);

        let to = tempfile::tempdir().unwrap();
        let Got::Received(got) = receive(synced.path(), &pass, to.path(), false).unwrap() else { panic!("all there") };
        assert_eq!(got.len(), 2);
        assert_eq!(std::fs::read(to.path().join("graph.bin")).unwrap().len(), 300_000);
        assert!(!synced.path().join(&pass.rel).exists() && !synced.path().join("Seedwell Pass-Through/kglite").exists(), "the pass is gone");
        assert!(left(std::slice::from_ref(&folder)).is_empty());

        // Not there yet (still syncing): it says which.
        let Got::Waiting(w) = receive(synced.path(), &pass, to.path(), false).unwrap() else { panic!("nothing there") };
        assert_eq!(w, ["graph.bin", "notes.md"]);
    }
}
