//! Software: installers, bundles and archives, each with its build variant.

use std::collections::BTreeMap;

use super::variant::{Cpu, Os, PkgKind, Variant};
use super::{
    Build, Ctx, Group, GroupKind, Item, StructureKind, attach, last_comp, plain_title, slug,
    stem_key,
};
use crate::classifier::{classify, clean_title};
use crate::nameinfo::NameInfo;
use crate::profile::FClass;
use crate::names::split_name;
use crate::types::{Category, Classification, FileEntry, Input};

/// The `.app` folder that contains a path, as `(root path, bundle name)`.
fn bundle_of(path: &str) -> Option<(&str, &str)> {
    let mut off = 0;
    for comp in path.split('/') {
        let end = off + comp.len();
        if comp.len() > 4 && comp.to_ascii_lowercase().ends_with(".app") && end < path.len() {
            return Some((&path[..end], comp));
        }
        off = end + 1;
    }
    None
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum K {
    Bundle(String),
    File(String, String),
}

const GENERIC: [&str; 9] = [
    "setup",
    "install",
    "installer",
    "installation",
    "autorun",
    "app",
    "launcher",
    "update",
    "patch",
];

pub(super) fn build(cx: &mut Ctx<'_>, c: &Classification) -> (StructureKind, Vec<Group>) {
    let mut keyed: BTreeMap<K, Vec<usize>> = BTreeMap::new();
    for i in 0..cx.f.len() {
        if !cx.pending(i) {
            continue;
        }
        if let Some((root, _)) = bundle_of(cx.f[i].path) {
            keyed.entry(K::Bundle(root.to_owned())).or_default().push(i);
        } else if matches!(
            cx.f[i].class,
            FClass::Software | FClass::Disc | FClass::Archive
        ) {
            let f = &cx.f[i];
            keyed
                .entry(K::File(f.dir.to_owned(), stem_key(f.base)))
                .or_default()
                .push(i);
        }
    }
    let mut builds = Vec::new();
    let mut meta: Vec<(Variant, String)> = Vec::new();
    for (k, idx) in keyed {
        let lead = idx
            .iter()
            .copied()
            .max_by_key(|&i| (cx.f[i].size, std::cmp::Reverse(i)))
            .unwrap_or(idx[0]);
        let files = cx.claim(idx.iter().map(|&i| i as u32).collect());
        if files.is_empty() {
            continue;
        }
        let (root, dir, stem, variant, name) = match &k {
            K::Bundle(r) => {
                let name = last_comp(r).to_owned();
                (
                    Some(r.clone()),
                    super::parent_of(r).to_owned(),
                    String::new(),
                    Variant::detect(r, "app", true),
                    name,
                )
            }
            K::File(d, s) => {
                let f = &cx.f[lead];
                (
                    None,
                    d.clone(),
                    s.clone(),
                    Variant::detect(f.path, &f.ext, false),
                    f.base.to_owned(),
                )
            }
        };
        meta.push((variant, name));
        builds.push(Build {
            files,
            root,
            dir,
            stem,
            lead: lead as u32,
        });
    }
    attach(cx, &mut builds, |_| true);

    let fallback = c
        .fields
        .name
        .as_deref()
        .or(c.fields.title.as_deref())
        .map(slug)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    let mut items: Vec<Item> = Vec::with_capacity(builds.len());
    for (b, (variant, name)) in builds.iter().zip(meta) {
        let product = product_slug(&name, &fallback);
        let mut files = b.files.clone();
        files.sort_unstable();
        files.dedup();
        let bytes = cx.bytes_of(&files);
        let mut it = Item::new(
            plain_title(&name),
            format!("sw:{product}:{}", variant.key()),
            files,
            bytes,
        );
        it.variant = Some(variant);
        items.push(it);
    }
    let os_order = |o: Os| match o {
        Os::MacOs => 0,
        Os::Windows => 1,
        Os::Linux => 2,
        Os::Android => 3,
        Os::Ios => 4,
        Os::Any => 5,
    };
    items.sort_by(|a, b| {
        let (va, vb) = (
            a.variant.unwrap_or(any_variant()),
            b.variant.unwrap_or(any_variant()),
        );
        (os_order(va.os), va.cpu, va.kind, &a.identity).cmp(&(
            os_order(vb.os),
            vb.cpu,
            vb.kind,
            &b.identity,
        ))
    });
    let mut groups: Vec<Group> = Vec::new();
    for it in items {
        let os = it.variant.map_or(Os::Any, |v| v.os);
        match groups.last_mut() {
            Some(g) if g.kind == GroupKind::Platform(os) => g.items.push(it),
            _ => groups.push(Group {
                label: os.display().to_owned(),
                kind: GroupKind::Platform(os),
                items: vec![it],
            }),
        }
    }
    (StructureKind::Software, groups)
}

fn any_variant() -> Variant {
    Variant {
        os: Os::Any,
        cpu: Cpu::Any,
        kind: PkgKind::Other,
    }
}

/// Product name key of an installer: the product name the classifier finds in the file name
/// (up to the version / platform words), or the torrent-level name for generic installer
/// names (`setup.exe`).
fn product_slug(file_name: &str, fallback: &str) -> String {
    let base = file_name
        .strip_suffix(".app")
        .or_else(|| file_name.strip_suffix(".App"))
        .unwrap_or(file_name);
    let files = [FileEntry {
        path: file_name,
        size: 1,
    }];
    let r = classify(&Input::new(base, &files));
    let mut t = if r.category == Category::Software {
        r.fields.name.unwrap_or_default()
    } else {
        String::new()
    };
    if t.is_empty() {
        let ni = NameInfo::parse(split_name(base).0, true);
        t = clean_title(&ni, ni.raw_title());
    }
    let s = slug(&t);
    if s.is_empty() || GENERIC.contains(&s.as_str()) {
        fallback.to_owned()
    } else {
        s
    }
}
