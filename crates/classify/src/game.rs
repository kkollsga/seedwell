//! Games, conservatively. A game is never decided from the files alone when they look like an ordinary program,
//! and never from a name alone when the content is video, audio or a book: [`looks_like_game`] is only asked
//! about a torrent that the classifier already placed in Software or Other.
//!
//! Evidence, any one of which is enough:
//! * the name: a bracketed repack (`[Some Repack]`), a scene-style `-GROUP` suffix next to a game word
//!   (`Update`, `Patch`, `DLC`, `GOTY`, `Early Access`...), a console tag (`PS5`, `PS4`, `NSW`, `XBOX360`,
//!   `Wii U`), or `Switch` next to `NSP`/`XCI`/`Nintendo`. No group is known by name;
//! * the files: `.nsp` / `.xci` (Switch), `eboot.bin` (PlayStation), `steam_api.dll`, a `setup.exe` with two or more
//!   `.bin` data discs (store and repack installers), or a pile of engine archives (`.pak`, `.vpk`, `.bsa`, `.unity3d`);
//! * a tracker category that says games.
//!
//! A program that carries one of those words in a legitimate way is kept out by [`APP_WORDS`].

use crate::types::{FileEntry, Input};

/// Words that mark a release as a game when the name also ends in a scene-style `-GROUP`.
const GAME_WORDS: &[&str] = &["update", "patch", "hotfix", "dlc", "dlcs", "goty", "campaign", "deluxe"];

/// Words of programs: a scene-style name that carries one is not a game.
const SOFT_WORDS: &[&str] = &[
    "macos", "windows", "x64", "x86", "x86_64", "arm64", "universal", "linux", "multilingual", "portable", "keygen", "crack",
    "cracked", "serial", "activated", "preactivated",
];

/// Console tags that stand for a game in a release name.
const PLATFORMS: &[&str] = &["ps5", "ps4", "ps3", "ps2", "psp", "psvita", "nsw", "xbox360", "xboxone", "wiiu", "gamecube"];

/// Words of programs that are not games: they veto the weaker evidence.
const APP_WORDS: &[&str] = &[
    "adobe", "autodesk", "photoshop", "office", "autocad", "ableton", "vst", "vst3", "aax", "antivirus", "ubuntu", "debian",
    "fedora", "netinst", "macos", "dmg", "premiere", "illustrator", "windows11", "windows10", "kaspersky", "nero",
];

fn words(name: &str) -> Vec<String> {
    name.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_ascii_lowercase).collect()
}

fn ext(path: &str) -> String {
    path.rsplit('/').next().and_then(|n| n.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

fn file_name(f: &FileEntry<'_>) -> String {
    f.path.rsplit('/').next().unwrap_or(f.path).to_ascii_lowercase()
}

/// The torrent is, with good reason, a game. See the module text.
pub(crate) fn looks_like_game(input: &Input<'_>) -> bool {
    let w = words(input.name);
    let app = w.iter().any(|x| APP_WORDS.contains(&x.as_str()));

    // The tracker's own category.
    if let Some(h) = input.tracker_source_hint {
        let h = h.to_ascii_lowercase();
        if (h.contains("game") || h.contains("repack")) && !app {
            return true;
        }
    }

    // The release group, by position: a scene-style `-GROUP` suffix next to a game word, or a bracket that says "repack".
    let name = input.name.trim();
    let tail = name.rsplit('-').next().map(words).unwrap_or_default();
    let group_suffix = name.contains('-') && tail.len() == 1 && (2..=12).contains(&tail[0].len());
    let early_access = w.iter().any(|x| x == "early") && w.iter().any(|x| x == "access");
    let game_word = early_access || w.iter().any(|x| GAME_WORDS.contains(&x.as_str()));
    let soft_word = w.iter().any(|x| SOFT_WORDS.contains(&x.as_str()));
    let scene = group_suffix && game_word && !soft_word;
    let repack = name.split(['[', '(']).skip(1).filter_map(|s| s.split([']', ')']).next()).any(|b| words(b).iter().any(|x| x == "repack"));
    let platform = w.iter().any(|x| PLATFORMS.contains(&x.as_str()));
    let switch = w.iter().any(|x| x == "switch") && (w.iter().any(|x| matches!(x.as_str(), "nsp" | "xci" | "nintendo" | "nsw")));

    // The files.
    let exts: Vec<String> = input.files.iter().map(|f| ext(f.path)).collect();
    let n = |e: &str| exts.iter().filter(|x| x.as_str() == e).count();
    let names: Vec<String> = input.files.iter().map(file_name).collect();
    let steam = names.iter().any(|f| f == "steam_api.dll" || f == "steam_api64.dll" || f == "goggame.dll");
    let console_files = n("nsp") + n("xci") > 0 || names.iter().any(|f| f == "eboot.bin");
    let installer_discs = names.iter().any(|f| f == "setup.exe" || f.starts_with("setup_") && f.ends_with(".exe")) && n("bin") >= 2;
    let engine_archives = n("pak") + n("vpk") + n("bsa") + n("unity3d") + n("uasset") + n("pck") >= 3;

    let strong_name = scene || repack || platform || switch;
    if app && !(platform || switch || steam || console_files) {
        return false;
    }
    strong_name || steam || console_files || installer_discs || engine_archives
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(name: &str, files: &[&str]) -> bool {
        let entries: Vec<FileEntry<'_>> = files.iter().map(|p| FileEntry { path: p, size: 1_000_000 }).collect();
        looks_like_game(&Input::new(name, &entries))
    }

    #[test]
    fn releases_of_games() {
        assert!(game("Neonpunk.2099.v2.1-BASALT", &["setup_neonpunk_2099.exe", "setup_neonpunk_2099-1.bin", "setup_neonpunk_2099-2.bin"]));
        assert!(game("Alder.Ring.Update.v1.10-ZEPHYR", &["zephyr.nfo", "update.exe"]));
        assert!(game("Hollow Hearth II [Foxhound Repack]", &["setup.exe", "fh-01.bin", "fh-02.bin"]));
        assert!(game("Marlo.Kart.8.Deluxe.NSW-SIXXERS", &["mk8x.nsp"]));
        assert!(game("Giant.of.War.PS4-DUPLET", &["a.pkg"]));
        assert!(game("Hollow Knave Switch NSP", &["hk.nsp"]));
        assert!(game("Some Folder", &["Game/steam_api64.dll", "Game/game.exe"]));
        assert!(game("Stuff", &["setup.exe", "disc-1.bin", "disc-2.bin"]));
        assert!(game("Stuff", &["data/a.pak", "data/b.pak", "data/c.pak", "game.exe"]));
        let mut i = Input::new("Whatever", &[]);
        i.tracker_source_hint = Some("PC Games");
        assert!(looks_like_game(&i));
    }

    #[test]
    fn ordinary_programs_and_titles_are_not_games() {
        assert!(!game("Antivirus.Pro.2024.v25.0-ZEPHYR", &["setup.exe", "a.bin", "b.bin"]), "an app from a scene-style name stays an app");
        assert!(!game("ubuntu-24.04-desktop-amd64.iso", &["ubuntu-24.04-desktop-amd64.iso"]));
        assert!(!game("Bureau.Office.2021.Pro.Plus", &["setup.exe"]));
        assert!(!game("Blender-4.2.0-windows-x64.msi", &["Blender-4.2.0-windows-x64.msi"]));

        assert!(!game("Reloaded Tools 1.2", &["reloaded.exe"]), "'reloaded' in a title is not the group");
        assert!(!game("Setup", &["setup.exe"]), "a lone installer is a program");
        assert!(!game("Switch Hosts 3.0", &["switchhosts.dmg"]), "'switch' alone is nothing");
        assert!(!game("My Tools", &["a.pak", "b.pak"]), "two archives are not a game");
        assert!(!game("Photo Archive", &["a.jpg"]));
    }
}
