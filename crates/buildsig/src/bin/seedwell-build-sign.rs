//! The development-build signer (SPEC 12A), run by `tools/build-sign.sh`, which keeps the secret key in the Mac's
//! Keychain and hands it over on standard input (never as an argument, never in a log).
//!
//! ```text
//! seedwell-build-sign keygen                      a new secret key (hex) on standard output
//! seedwell-build-sign public                      the public key of the secret key on standard input
//! seedwell-build-sign add <folder> <zip> --sha256 HEX [--commit C] [--minimum V]
//!                                                 sign <folder>/build.json with <zip> in it (secret key on stdin)
//! seedwell-build-sign verify <folder> [--key HEX] check <folder>/build.json against the trusted keys (or HEX)
//! ```

use std::io::Read as _;
use std::path::Path;
use std::process::ExitCode;

use seedwell_buildsig as sig;

const USAGE: &str = "usage: seedwell-build-sign keygen | public | add <folder> <zip> --sha256 HEX [--commit C] [--minimum V] | verify <folder> [--key HEX]";

fn secret_from_stdin() -> Result<sig::SigningKey, String> {
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s).map_err(|e| format!("standard input: {e}"))?;
    sig::SigningKey::from_seed_hex(&s).ok_or_else(|| "standard input is not a build key (64 hex characters)".to_owned())
}

/// The value after `--name`, once.
fn opt(args: &[String], name: &str) -> Result<Option<String>, String> {
    let at: Vec<usize> = args.iter().enumerate().filter(|(_, a)| *a == name).map(|(i, _)| i).collect();
    match at.as_slice() {
        [] => Ok(None),
        [i] => args.get(i + 1).cloned().map(Some).ok_or_else(|| format!("{name} needs a value")),
        _ => Err(format!("{name} given twice")),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn run(args: &[String]) -> Result<(), String> {
    let positional: Vec<&String> = {
        let mut out = Vec::new();
        let mut skip = false;
        for a in args {
            if skip {
                skip = false;
            } else if a.starts_with("--") {
                skip = true;
            } else {
                out.push(a);
            }
        }
        out
    };
    match positional.as_slice() {
        [c] if c.as_str() == "keygen" && args.len() == 1 => {
            let (k, seed) = sig::SigningKey::generate()?;
            eprintln!("public key {}", sig::hex(&k.public_key()));
            println!("{seed}");
            Ok(())
        }
        [c] if c.as_str() == "public" && args.len() == 1 => {
            println!("{}", sig::hex(&secret_from_stdin()?.public_key()));
            Ok(())
        }
        [c, folder, zip] if c.as_str() == "add" => {
            let sha256 = opt(args, "--sha256")?.ok_or("add needs --sha256 (the zip's, as its builder reported it)")?;
            let commit = opt(args, "--commit")?;
            let minimum = opt(args, "--minimum")?;
            let key = secret_from_stdin()?;
            let a = sig::Adding { zip, sha256: &sha256, commit: commit.as_deref(), minimum: minimum.as_deref() };
            let m = sig::add(Path::new(folder.as_str()), &a, &key, &sig::trusted_keys(), now_ms())?;
            for (system, b) in &m.builds {
                println!("{system}: {} {} {}", b.version, b.zip, b.sha256);
            }
            if !sig::trusted_keys().contains(&key.public_key()) {
                eprintln!(
                    "note: this key ({}) is not among the keys this tree's Seedwell trusts (seedwell-buildsig TRUSTED_KEYS)",
                    sig::hex(&key.public_key())
                );
            }
            Ok(())
        }
        [c, folder] if c.as_str() == "verify" => {
            let keys = match opt(args, "--key")? {
                Some(k) => vec![sig::parse_key(&k).ok_or("--key is not a public key (64 hex characters)")?],
                None => sig::trusted_keys(),
            };
            let doc = std::fs::read(Path::new(folder.as_str()).join(sig::FILE)).map_err(|e| format!("{}: {e}", sig::FILE))?;
            let (m, by) = sig::verify(&doc, &keys).map_err(|e| e.to_string())?;
            println!("signed by {}", sig::hex(&by));
            for (system, b) in &m.builds {
                let there = sig::sha256_file(&Path::new(folder.as_str()).join(&b.zip)).ok();
                let state = match there {
                    Some(h) if h == b.sha256 => "zip matches",
                    Some(_) => "zip differs",
                    None => "zip missing",
                };
                println!("{system}: {} {} {} ({state})", b.version, b.zip, b.sha256);
            }
            Ok(())
        }
        _ => Err(USAGE.to_owned()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("seedwell-build-sign: {e}");
            ExitCode::FAILURE
        }
    }
}
