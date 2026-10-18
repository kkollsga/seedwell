//! `diff`: semantic comparison of two captures (client A vs client B).

use crate::analyze::{self, Profile, groups, profile, rle};
use crate::record::read_records;
use anyhow::{Result, bail};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::PathBuf;

/// Arguments for `diff`.
#[derive(clap::Args, Debug)]
pub struct Args {
    /// First capture (reference).
    pub a: PathBuf,
    /// Second capture (candidate).
    pub b: PathBuf,
}

fn first_profile(path: &std::path::Path) -> Result<Vec<Profile>> {
    let (recs, _) = read_records(path)?;
    let ps: Vec<Profile> = groups(&recs).iter().map(profile).collect();
    if ps.is_empty() {
        bail!("{} contains no announce records", path.display());
    }
    Ok(ps)
}

fn line(s: &mut String, what: &str, a: &str, b: &str) {
    if a == b {
        let _ = writeln!(s, "  SAME  {what}: {a}");
    } else {
        let _ = writeln!(s, "  DIFF  {what}:\n          A: {a}\n          B: {b}");
    }
}

fn values(p: &Profile, k: &str) -> String {
    p.param_values.get(k).map_or_else(|| "<absent>".into(), |m| m.keys().cloned().collect::<Vec<_>>().join("|"))
}

fn orders(p: &Profile, ev: &str) -> String {
    p.order_by_event.get(ev).and_then(|v| v.first()).map_or_else(|| "<no such event>".into(), |(o, _)| o.join("&"))
}

fn counters_semantics(p: &Profile) -> String {
    if p.handoffs.is_empty() {
        return "no restart/resume observed".into();
    }
    p.handoffs
        .iter()
        .map(|h| {
            let reset = h.after.0 == 0 && h.after.1 == 0 && (h.before.0 != 0 || h.before.1 != 0);
            format!("{} -> {}", h.ended_with, if reset { "reset to 0" } else { "carried over" })
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Compare one pair of profiles.
pub fn compare(a: &Profile, b: &Profile) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "A = {} / {}   B = {} / {}", a.client, &a.info_hash[..a.info_hash.len().min(8)], b.client, &b.info_hash[..b.info_hash.len().min(8)]);
    line(&mut s, "event sequence", &rle(&a.events), &rle(&b.events));
    let ua = |p: &Profile| p.user_agents.keys().cloned().collect::<Vec<_>>().join(" | ");
    line(&mut s, "User-Agent", &ua(a), &ua(b));
    let mut evs: BTreeSet<&String> = a.order_by_event.keys().collect();
    evs.extend(b.order_by_event.keys());
    for ev in evs {
        line(&mut s, &format!("param order [{ev}]"), &orders(a, ev), &orders(b, ev));
    }
    let keys = |p: &Profile| p.param_values.keys().cloned().collect::<BTreeSet<_>>();
    let (ka, kb) = (keys(a), keys(b));
    let only_a: Vec<_> = ka.difference(&kb).cloned().collect();
    let only_b: Vec<_> = kb.difference(&ka).cloned().collect();
    line(&mut s, "parameters only in one client", &format!("only A: {only_a:?}"), &format!("only B: {only_b:?}"));
    for k in ["numwant", "compact", "supportcrypto", "requirecrypto", "no_peer_id", "corrupt", "redundant", "ip", "ipv4", "ipv6", "trackerid"] {
        line(&mut s, &format!("values of {k}"), &values(a, k), &values(b, k));
    }
    let enc = |p: &Profile| format!("hex case {}, needless escapes {}, unsafe literals {}", p.enc_info_hash.hex_case, p.enc_info_hash.unreserved_escaped, p.enc_info_hash.reserved_literal);
    line(&mut s, "info_hash encoding", &enc(a), &enc(b));
    line(&mut s, "counters across sessions", &counters_semantics(a), &counters_semantics(b));
    let stab = |p: &Profile| {
        let k: BTreeSet<_> = p.sessions.iter().flat_map(|x| x.keys.iter()).collect();
        let pid: BTreeSet<_> = p.sessions.iter().flat_map(|x| x.peer_ids.iter()).collect();
        format!("{} session(s), {} distinct key(s), {} distinct peer_id(s)", p.sessions.len(), k.len(), pid.len())
    };
    line(&mut s, "key / peer_id stability", &stab(a), &stab(b));
    let gap = |p: &Profile| {
        if p.gaps.is_empty() {
            "no regular announces".to_owned()
        } else {
            let ratios: Vec<f64> = p.gaps.iter().filter_map(|(g, iv, _)| iv.map(|i| g / i as f64)).collect();
            let avg = ratios.iter().sum::<f64>() / ratios.len().max(1) as f64;
            format!("{} gaps, mean gap/interval = {avg:.2}", p.gaps.len())
        }
    };
    line(&mut s, "interval adherence", &gap(a), &gap(b));
    s
}

/// Entry point.
pub fn run(a: &Args) -> Result<()> {
    let pa = first_profile(&a.a)?;
    let pb = first_profile(&a.b)?;
    let mut used = BTreeSet::new();
    let mut n = 0;
    for x in &pa {
        // Prefer the same torrent; otherwise pair first-with-first.
        let y = pb.iter().enumerate().find(|(i, y)| y.info_hash == x.info_hash && !used.contains(i)).or_else(|| pb.iter().enumerate().find(|(i, _)| !used.contains(i)));
        if let Some((i, y)) = y {
            used.insert(i);
            n += 1;
            println!("# Comparison {n}\n{}", compare(x, y));
        }
    }
    let _ = analyze::fmt_t(0);
    Ok(())
}
