//! Play while downloading (SPEC §7F), the part that needs no engine support: which files are
//! video, and the checks `stream_url` makes before it can answer.

use trs_stream::mime;

use crate::types::{CoreError, FilePriority, TorrentFile};

/// A file by its place in the torrent's list. `padding` files are never listed.
pub(crate) struct FileFacts<'a> {
    pub path: &'a str,
    /// Absolute location on disk.
    pub disk: &'a str,
    pub length: u64,
    pub done: u64,
    pub priority: FilePriority,
    pub padding: bool,
    /// Payload bytes served from this file since the torrent went live.
    pub uploaded: u64,
}

/// The video check: the extension maps to a `video/*` content type.
pub(crate) fn is_video(name: &str) -> bool {
    mime::content_type(name).starts_with("video/")
}

/// MP4, M4V, MOV (what QuickTime Player opens). Audio-only M4A is not video.
pub(crate) fn is_mp4_family(name: &str) -> bool {
    is_video(name) && mime::is_mp4_family(name)
}

/// The list the app shows: padding files left out, indices kept.
pub(crate) fn list(files: &[FileFacts<'_>]) -> Vec<TorrentFile> {
    files
        .iter()
        .enumerate()
        .filter(|(_, f)| !f.padding)
        .map(|(i, f)| {
            let name = f.path.rsplit('/').next().unwrap_or(f.path);
            TorrentFile {
                index: u32::try_from(i).unwrap_or(u32::MAX),
                path: f.path.to_owned(),
                disk_path: f.disk.to_owned(),
                name: name.to_owned(),
                length: f.length,
                done: f.done,
                uploaded: f.uploaded,
                wanted: f.priority != FilePriority::Skip,
                priority: f.priority,
                is_video: is_video(name),
                is_mp4_family: is_mp4_family(name),
            }
        })
        .collect()
}

/// The tree path and disk location of each engine file. The engine lists a file as the path it is
/// configured with: relative to the download folder and starting with the torrent's folder, or absolute
/// when it was placed into the library. The tree shows the torrent's folders without the root, and for
/// placed files the folders below what all placed files share (a skipped file keeps its torrent path).
pub(crate) fn tree_paths(paths: &[String], save_path: &std::path::Path) -> Vec<(String, String)> {
    use std::path::{Component, Path, PathBuf};
    let parent = save_path.parent().unwrap_or(save_path);
    let abs: Vec<&Path> = paths.iter().map(Path::new).filter(|p| p.is_absolute()).collect();
    // The deepest folder that holds every placed file.
    let common: Option<PathBuf> = abs.split_first().map(|(first, rest)| {
        let mut c: Vec<Component<'_>> = first.parent().unwrap_or(first).components().collect();
        for p in rest {
            let n = c.iter().zip(p.parent().unwrap_or(p).components()).take_while(|(a, b)| *a == b).count();
            c.truncate(n);
        }
        c.into_iter().collect()
    });
    paths
        .iter()
        .map(|raw| {
            let p = Path::new(raw);
            if p.is_absolute() {
                let rel = common.as_deref().and_then(|c| p.strip_prefix(c).ok()).unwrap_or(p);
                // A tree label: `/` between folders on every system, like the torrent's own paths.
                let label: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
                (label.join("/"), raw.clone())
            } else {
                let rel = match raw.split_once('/') {
                    Some((_, rest)) => rest.to_owned(),
                    None => raw.clone(),
                };
                // The location on disk, with this system's separators: a torrent path is `/`-separated.
                let full = raw.split('/').fold(parent.to_path_buf(), |p, c| p.join(c));
                (rel, full.to_string_lossy().into_owned())
            }
        })
        .collect()
}

/// What `stream_url` checks before anything else: the file exists and is a video. A file that was
/// skipped is fine: the engine selects it for download when it starts the stream (SPEC §7F.2).
pub(crate) fn check_streamable(files: &[TorrentFile], index: u32) -> Result<&TorrentFile, CoreError> {
    let f = files
        .iter()
        .find(|f| f.index == index)
        .ok_or_else(|| CoreError::Failed { message: format!("the torrent has no file {index}") })?;
    if !f.is_video {
        return Err(CoreError::Failed { message: format!("“{}” isn't a video file.", f.name) });
    }
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(path: &str) -> FileFacts<'_> {
        FileFacts { path, disk: path, length: 100, done: 0, priority: FilePriority::Normal, padding: false, uploaded: 0 }
    }

    #[test]
    fn video_and_mp4_family_by_extension() {
        for (n, v, m) in [
            ("Movie.2021.1080p.mkv", true, false),
            ("clip.MP4", true, true),
            ("clip.m4v", true, true),
            ("clip.mov", true, true),
            ("show.S01E01.avi", true, false),
            ("song.m4a", false, false),
            ("song.flac", false, false),
            ("readme.nfo", false, false),
            ("noextension", false, false),
        ] {
            assert_eq!((is_video(n), is_mp4_family(n)), (v, m), "{n}");
        }
    }

    #[test]
    fn list_skips_padding_but_keeps_indices() {
        let mut pad = facts(".pad/4096");
        pad.padding = true;
        let files = [facts("S01/E01.mkv"), pad, facts("S01/E02.mkv"), facts("S01/info.nfo")];
        let l = list(&files);
        assert_eq!(l.iter().map(|f| f.index).collect::<Vec<_>>(), [0, 2, 3]);
        assert_eq!(l[0].name, "E01.mkv");
        assert!(l[1].is_video && !l[2].is_video);
    }

    #[test]
    fn tree_paths_strip_the_root_and_the_shared_library_folder() {
        // Absolute on this system (a drive in front on Windows).
        let abs = |p: &str| if cfg!(windows) { format!("C:{p}") } else { p.to_string() };
        let save = std::path::PathBuf::from(abs("/dl/Pack.S01"));
        let r = tree_paths(&["Pack.S01/Season 1/e1.mkv".into(), "Pack.S01/Extras/s.mkv".into()], &save);
        let full = std::path::Path::new(&abs("/dl")).join("Pack.S01").join("Season 1").join("e1.mkv");
        assert_eq!(r[0], ("Season 1/e1.mkv".into(), full.display().to_string()));
        assert_eq!(r[1].0, "Extras/s.mkv");
        let placed = tree_paths(
            &[abs("/lib/Show/Season 01/Show - E1.mkv"), abs("/lib/Show/Season 01/Sub/E1.srt"), "Pack.S01/Extras/s.mkv".into()],
            &save,
        );
        assert_eq!(placed[0], ("Show - E1.mkv".into(), abs("/lib/Show/Season 01/Show - E1.mkv")));
        assert_eq!(placed[1].0, "Sub/E1.srt", "a tree label has / on every system");
        assert_eq!(placed[2].0, "Extras/s.mkv");
        // One file: just its name; a flat single-file torrent keeps its name.
        assert_eq!(tree_paths(&[abs("/lib/m/Movie.mkv")], &save)[0].0, "Movie.mkv");
        assert_eq!(tree_paths(&["Movie.mkv".into()], &save)[0].0, "Movie.mkv");
    }

    #[test]
    fn streamable_means_an_existing_video() {
        let mut skipped = facts("b.mkv");
        skipped.priority = FilePriority::Skip;
        let l = list(&[facts("a.mkv"), skipped, facts("c.nfo")]);
        assert!(check_streamable(&l, 0).is_ok());
        // A skipped video can be streamed: the engine selects it.
        assert!(check_streamable(&l, 1).is_ok());
        assert!(matches!(check_streamable(&l, 2), Err(CoreError::Failed { .. })));
        assert!(matches!(check_streamable(&l, 9), Err(CoreError::Failed { .. })));
    }
}
