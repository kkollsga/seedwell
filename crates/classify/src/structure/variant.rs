//! Software build variants: operating system, CPU, package kind.

/// Operating system a build targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Os {
    /// macOS (`.dmg`, `.pkg`, `.app`, "mac", "osx").
    MacOs,
    /// Windows (`.exe`, `.msi`, "win64").
    Windows,
    /// Linux (`.AppImage`, `.deb`, `.rpm`).
    Linux,
    /// Android (`.apk`).
    Android,
    /// iOS (`.ipa`).
    Ios,
    /// Not stated (a bare `.zip` or `.iso`).
    Any,
}

impl Os {
    /// Lower-case key used in identities.
    pub const fn as_str(self) -> &'static str {
        match self {
            Os::MacOs => "macos",
            Os::Windows => "windows",
            Os::Linux => "linux",
            Os::Android => "android",
            Os::Ios => "ios",
            Os::Any => "any",
        }
    }

    /// Display name (`macOS`, `Windows`, ...; `Other` for [`Os::Any`]).
    pub const fn display(self) -> &'static str {
        match self {
            Os::MacOs => "macOS",
            Os::Windows => "Windows",
            Os::Linux => "Linux",
            Os::Android => "Android",
            Os::Ios => "iOS",
            Os::Any => "Other",
        }
    }
}

/// CPU architecture a build targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Cpu {
    /// `arm64`, `aarch64`, "Apple Silicon", `M1`.
    Arm64,
    /// `x86_64`, `amd64`, `x64`, "Intel".
    X86_64,
    /// `universal`, or both arm64 and x86_64 named.
    Universal,
    /// 32-bit x86 (`i386`, `x86`, `win32`).
    X86,
    /// Not stated.
    Any,
}

impl Cpu {
    /// Lower-case key used in identities.
    pub const fn as_str(self) -> &'static str {
        match self {
            Cpu::Arm64 => "arm64",
            Cpu::X86_64 => "x86_64",
            Cpu::Universal => "universal",
            Cpu::X86 => "x86",
            Cpu::Any => "any",
        }
    }

    /// Preference on an Apple-silicon Mac: native first.
    pub(crate) const fn mac_rank(self) -> u8 {
        match self {
            Cpu::Arm64 => 0,
            Cpu::Universal => 1,
            _ => 2,
        }
    }
}

/// Package / container type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PkgKind {
    /// Disk image.
    Dmg,
    /// macOS installer package (`.pkg`, `.mpkg`).
    Pkg,
    /// Windows executable.
    Exe,
    /// Windows installer (`.msi`, `.msix`).
    Msi,
    /// Linux AppImage.
    AppImage,
    /// Debian package.
    Deb,
    /// RPM package.
    Rpm,
    /// Android package.
    Apk,
    /// Disc image.
    Iso,
    /// Archive (`.zip`, `.7z`, `.rar`, `.tar.gz`).
    Zip,
    /// `.app` bundle folder, `.ipa`, anything else.
    Other,
}

impl PkgKind {
    /// Lower-case key used in identities.
    pub const fn as_str(self) -> &'static str {
        match self {
            PkgKind::Dmg => "dmg",
            PkgKind::Pkg => "pkg",
            PkgKind::Exe => "exe",
            PkgKind::Msi => "msi",
            PkgKind::AppImage => "appimage",
            PkgKind::Deb => "deb",
            PkgKind::Rpm => "rpm",
            PkgKind::Apk => "apk",
            PkgKind::Iso => "iso",
            PkgKind::Zip => "zip",
            PkgKind::Other => "other",
        }
    }

    /// Which package to prefer when several fit: disk images before installers before archives.
    pub(crate) const fn rank(self) -> u8 {
        match self {
            PkgKind::Dmg => 0,
            PkgKind::Pkg => 1,
            PkgKind::Zip => 2,
            PkgKind::Other => 3,
            _ => 4,
        }
    }
}

/// One software build.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Variant {
    /// Operating system.
    pub os: Os,
    /// CPU architecture.
    pub cpu: Cpu,
    /// Package kind.
    pub kind: PkgKind,
}

impl Variant {
    /// Runs natively on this Mac (Apple silicon, macOS): a macOS build for arm64, universal or
    /// an unspecified CPU. An x86_64-only build (Rosetta) and every non-macOS build are not.
    pub fn is_this_mac(&self) -> bool {
        self.os == Os::MacOs && matches!(self.cpu, Cpu::Arm64 | Cpu::Universal | Cpu::Any)
    }

    /// `macos-arm64-dmg`: the variant part of a software identity.
    pub fn key(&self) -> String {
        format!(
            "{}-{}-{}",
            self.os.as_str(),
            self.cpu.as_str(),
            self.kind.as_str()
        )
    }

    /// Detect from a path (folders and file name, without the torrent name) and its extension.
    /// `bundle` marks a `.app` folder.
    pub fn detect(path: &str, ext: &str, bundle: bool) -> Variant {
        let lower = path.to_ascii_lowercase();
        let words: Vec<&str> = lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        let has = |w: &str| words.contains(&w);
        let bigram = |a: &str, b: &str| words.windows(2).any(|p| p[0] == a && p[1] == b);

        // --- extension decides the strong cases
        let (ext_os, kind) = if bundle {
            (Some(Os::MacOs), PkgKind::Other)
        } else {
            match ext {
                "dmg" => (Some(Os::MacOs), PkgKind::Dmg),
                "pkg" | "mpkg" => (Some(Os::MacOs), PkgKind::Pkg),
                "exe" => (Some(Os::Windows), PkgKind::Exe),
                "msi" | "msix" => (Some(Os::Windows), PkgKind::Msi),
                "appimage" => (Some(Os::Linux), PkgKind::AppImage),
                "deb" => (Some(Os::Linux), PkgKind::Deb),
                "rpm" => (Some(Os::Linux), PkgKind::Rpm),
                "snap" | "flatpak" => (Some(Os::Linux), PkgKind::Other),
                "apk" | "xapk" => (Some(Os::Android), PkgKind::Apk),
                "ipa" => (Some(Os::Ios), PkgKind::Other),
                "app" => (Some(Os::MacOs), PkgKind::Other),
                "iso" | "img" => (None, PkgKind::Iso),
                "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "r00"
                | "001" => (None, PkgKind::Zip),
                _ => (None, PkgKind::Other),
            }
        };

        // --- operating system from words
        let mut os_words = None;
        for w in &words {
            let o = match *w {
                "mac" | "macos" | "macosx" | "osx" | "darwin" | "macintosh" => Some(Os::MacOs),
                "win" | "win32" | "win64" | "windows" | "win10" | "win11" | "w64" => {
                    Some(Os::Windows)
                }
                "linux" | "ubuntu" | "debian" | "fedora" | "gnu" => Some(Os::Linux),
                "android" => Some(Os::Android),
                "ios" | "ipados" | "iphone" | "ipad" => Some(Os::Ios),
                w if w.starts_with("macos") || w.starts_with("osx") => Some(Os::MacOs),
                _ => None,
            };
            if o.is_some() {
                os_words = o;
                break;
            }
        }
        let os = ext_os.or(os_words).unwrap_or(Os::Any);

        // --- CPU from words
        let arm = has("arm64")
            || has("arm64e")
            || has("aarch64")
            || has("armv8")
            || has("applesilicon")
            || bigram("apple", "silicon")
            || has("silicon")
            || ["m1", "m2", "m3", "m4"].iter().any(|m| has(m));
        let x64 = has("amd64")
            || has("x64")
            || has("x8664")
            || has("win64")
            || has("intel")
            || bigram("x86", "64")
            || bigram("64", "bit")
            || has("64bit")
            || has("x64v2");
        let x86 = has("i386")
            || has("i686")
            || has("ia32")
            || has("win32")
            || has("x86") && !bigram("x86", "64")
            || has("32bit")
            || bigram("32", "bit");
        let universal = words.iter().any(|w| w.starts_with("universal"));
        let cpu = if universal || (arm && x64) {
            Cpu::Universal
        } else if arm {
            Cpu::Arm64
        } else if x64 {
            Cpu::X86_64
        } else if x86 {
            Cpu::X86
        } else {
            Cpu::Any
        };
        Variant { os, cpu, kind }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(path: &str) -> Variant {
        let ext = super::super::ext_of(path.rsplit('/').next().unwrap());
        Variant::detect(path, &ext, false)
    }

    #[test]
    fn mac_builds() {
        let a = v("App-1.2-arm64.dmg");
        assert_eq!((a.os, a.cpu, a.kind), (Os::MacOs, Cpu::Arm64, PkgKind::Dmg));
        assert!(a.is_this_mac());
        let b = v("macOS/App_1.2_x86_64.dmg");
        assert_eq!((b.os, b.cpu), (Os::MacOs, Cpu::X86_64));
        assert!(!b.is_this_mac());
        let u = v("App 1.2 Universal.pkg");
        assert_eq!((u.cpu, u.kind), (Cpu::Universal, PkgKind::Pkg));
        assert!(u.is_this_mac());
        let s = v("App.1.2.Apple.Silicon.dmg");
        assert_eq!(s.cpu, Cpu::Arm64);
        let g = v("App.dmg");
        assert!(g.is_this_mac(), "a generic dmg is for the Mac");
        assert_eq!(v("App-arm64-x86_64.dmg").cpu, Cpu::Universal);
    }

    #[test]
    fn other_systems() {
        let w = v("App-win64.exe");
        assert_eq!((w.os, w.cpu), (Os::Windows, Cpu::X86_64));
        assert_eq!(v("App.msi").os, Os::Windows);
        assert_eq!(v("Windows/x86/App.exe").cpu, Cpu::X86);
        let l = v("App-1.2-x86_64.AppImage");
        assert_eq!(
            (l.os, l.cpu, l.kind),
            (Os::Linux, Cpu::X86_64, PkgKind::AppImage)
        );
        assert_eq!(v("app_1.2_amd64.deb").cpu, Cpu::X86_64);
        assert_eq!(v("app-1.2.aarch64.rpm").cpu, Cpu::Arm64);
        assert_eq!(v("app.apk").os, Os::Android);
        assert!(!w.is_this_mac());
    }

    #[test]
    fn archives_take_the_os_from_their_name() {
        assert_eq!(v("App-macOS.zip").os, Os::MacOs);
        assert_eq!(v("source.zip").os, Os::Any);
        assert!(!v("source.zip").is_this_mac());
    }

    #[test]
    fn keys() {
        assert_eq!(v("a-arm64.dmg").key(), "macos-arm64-dmg");
    }
}
