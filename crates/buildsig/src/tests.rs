use super::*;

fn key(n: u8) -> SigningKey {
    SigningKey::from_seed_hex(&hex(&[n; 32])).unwrap()
}

fn build(v: &str, zip: &str, sha: &str) -> Build {
    Build { version: v.into(), commit: Some("abc1234".into()), zip: zip.into(), sha256: sha.into(), minimum_system: Some("15.0".into()) }
}

fn manifest() -> Manifest {
    let mut m = Manifest::new(1_000);
    m.builds.insert("macos".into(), build("0.1.12.3", "Seedwell-0.1.12.3.zip", &"ab".repeat(32)));
    m.builds.insert("windows".into(), build("0.1.12.2", "Seedwell-0.1.12.2-windows-x64.zip", &"CD".repeat(32)));
    m
}

#[test]
fn a_manifest_signed_by_a_trusted_key_verifies_and_reads_back() {
    let k = key(1);
    let doc = k.sign(&manifest());
    let (m, by) = verify(&doc, &[key(9).public_key(), k.public_key()]).unwrap();
    assert_eq!(by, k.public_key());
    let mut want = manifest();
    want.builds.get_mut("windows").unwrap().sha256 = "cd".repeat(32);
    assert_eq!(m, want, "SHA-256s are lower-cased");
    // The secret is not in the file; the public key is.
    let text = String::from_utf8(doc).unwrap();
    assert!(!text.contains(&hex(&[1u8; 32])));
    assert!(text.contains(&hex(&k.public_key())));
}

#[test]
fn a_key_that_is_not_trusted_is_refused() {
    let doc = key(2).sign(&manifest());
    assert_eq!(verify(&doc, &[key(1).public_key()]).unwrap_err(), Refused::UntrustedKey(hex(&key(2).public_key())));
    assert!(matches!(verify(&doc, &[]), Err(Refused::UntrustedKey(_))), "no trusted key: nothing verifies");
    assert!(verify(&doc, &[key(1).public_key()]).unwrap_err().to_string().contains("does not trust"));
}

/// A trusted key named in the file, with a signature that is not its: the file was changed or forged.
#[test]
fn a_changed_manifest_or_a_forged_signature_is_refused() {
    let k = key(1);
    let trusted = [k.public_key()];
    let doc = String::from_utf8(k.sign(&manifest())).unwrap();
    // Another zip's checksum put in after signing.
    let tampered = doc.replacen(&"ab".repeat(32), &"ef".repeat(32), 1);
    assert_ne!(tampered, doc);
    assert_eq!(verify(tampered.as_bytes(), &trusted).unwrap_err(), Refused::BadSignature);
    // Another key's signature over the same manifest, under the trusted key's name.
    let other: serde_json::Value = serde_json::from_slice(&key(2).sign(&manifest())).unwrap();
    let mut forged: serde_json::Value = serde_json::from_str(&doc).unwrap();
    forged["signature"] = other["signature"].clone();
    assert_eq!(verify(forged.to_string().as_bytes(), &trusted).unwrap_err(), Refused::BadSignature);
    // A signature over the bare manifest (without the context) is not a build key's signature either.
    let manifest_text = forged["manifest"].as_str().unwrap().to_owned();
    let bare = Ed25519KeyPair::from_seed_unchecked(&[1u8; 32]).unwrap().sign(manifest_text.as_bytes());
    forged["signature"] = hex(bare.as_ref()).into();
    assert_eq!(verify(forged.to_string().as_bytes(), &trusted).unwrap_err(), Refused::BadSignature);
}

#[test]
fn an_unsigned_or_garbled_document_is_refused() {
    let trusted = [key(1).public_key()];
    let latest = br#"{"version":"0.1.12.3","zip":"Seedwell-0.1.12.3.zip","sha256":"abababababababababababababababababababababababababababababababab"}"#;
    for doc in [&latest[..], b"", b"not json", br#"{"manifest":"{}","key":"00","signature":"00"}"#] {
        assert_eq!(verify(doc, &trusted).unwrap_err(), Refused::Unreadable, "{}", String::from_utf8_lossy(doc));
    }
    let big = vec![b' '; MAX_BYTES + 1];
    assert_eq!(verify(&big, &trusted).unwrap_err(), Refused::Unreadable);
}

#[test]
fn a_signed_manifest_with_paths_or_bad_fields_is_refused() {
    let k = key(1);
    let trusted = [k.public_key()];
    let bad = |f: &dyn Fn(&mut Build)| {
        let mut m = manifest();
        f(m.builds.get_mut("macos").unwrap());
        verify(&k.sign(&m), &trusted).unwrap_err()
    };
    for zip in ["../Seedwell-0.1.12.3.zip", "/tmp/Seedwell.zip", "a\\b.zip", ".Seedwell.zip", "Seedwell.tar"] {
        assert!(matches!(bad(&|b| b.zip = zip.into()), Refused::BadManifest(_)), "{zip}");
    }
    assert!(matches!(bad(&|b| b.sha256 = "12".into()), Refused::BadManifest(_)));
    assert!(matches!(bad(&|b| b.version = "1.2.3-beta".into()), Refused::BadManifest(_)));
    let mut m = manifest();
    m.format = 2;
    assert!(matches!(verify(&k.sign(&m), &trusted).unwrap_err(), Refused::BadManifest(_)));
}

#[test]
fn zip_names_say_their_system_and_version() {
    assert_eq!(zip_system("Seedwell-0.1.12.3.zip"), Some(("macos", "0.1.12.3".into())));
    assert_eq!(zip_system("Seedwell-0.1.12.3-windows-x64.zip"), Some(("windows", "0.1.12.3".into())));
    for bad in ["Seedwell-.zip", "Seedwell-0.1.12.3.tar", "Other-0.1.zip", "Seedwell-x.zip"] {
        assert_eq!(zip_system(bad), None, "{bad}");
    }
}

#[test]
fn the_seed_is_hex_and_gives_one_public_key() {
    assert!(SigningKey::from_seed_hex("00").is_none());
    assert!(SigningKey::from_seed_hex(&"zz".repeat(32)).is_none());
    let (k, seed) = SigningKey::generate().unwrap();
    assert_eq!(SigningKey::from_seed_hex(&format!(" {seed}\n")).unwrap().public_key(), k.public_key());
    assert_eq!(parse_key(&hex(&k.public_key())), Some(k.public_key()));
    assert!(trusted_keys().len() == TRUSTED_KEYS.len(), "every compiled-in key parses");
}

#[test]
fn adding_a_build_signs_it_with_the_other_systems_build_kept() {
    let d = tempfile::tempdir().unwrap();
    let k = key(1);
    let put = |name: &str, body: &[u8]| {
        std::fs::write(d.path().join(name), body).unwrap();
        sha256_file(&d.path().join(name)).unwrap()
    };
    let mac = put("Seedwell-0.1.12.3.zip", b"mac");
    let a = Adding { zip: "Seedwell-0.1.12.3.zip", sha256: &mac, commit: Some("abc1234"), minimum: Some("15.0") };
    add(d.path(), &a, &k, &[], 5).unwrap();
    let win = put("Seedwell-0.1.12.1-windows-x64.zip", b"windows");
    // The Windows zip as its builder reported it: a different file there is not signed.
    let wrong = Adding { zip: "Seedwell-0.1.12.1-windows-x64.zip", sha256: &mac, commit: None, minimum: None };
    assert!(add(d.path(), &wrong, &k, &[], 6).unwrap_err().contains("not signed"));
    let w = Adding { zip: "Seedwell-0.1.12.1-windows-x64.zip", sha256: &win.to_uppercase(), commit: None, minimum: None };
    add(d.path(), &w, &k, &[], 7).unwrap();
    let (m, _) = verify(&std::fs::read(d.path().join(FILE)).unwrap(), &[k.public_key()]).unwrap();
    assert_eq!(m.signed_ms, 7);
    assert_eq!(m.builds["macos"], Build { version: "0.1.12.3".into(), commit: Some("abc1234".into()), zip: "Seedwell-0.1.12.3.zip".into(), sha256: mac, minimum_system: Some("15.0".into()) });
    assert_eq!((m.builds["windows"].version.as_str(), m.builds["windows"].sha256.as_str()), ("0.1.12.1", win.as_str()));
    assert!(!d.path().join(format!(".{FILE}.part")).exists());

    // A newer Mac build whose older zip is gone: the Windows entry stays, the old Mac one is replaced.
    std::fs::remove_file(d.path().join("Seedwell-0.1.12.3.zip")).unwrap();
    let mac4 = put("Seedwell-0.1.12.4.zip", b"mac4");
    add(d.path(), &Adding { zip: "Seedwell-0.1.12.4.zip", sha256: &mac4, commit: None, minimum: None }, &k, &[], 8).unwrap();
    let (m, _) = verify(&std::fs::read(d.path().join(FILE)).unwrap(), &[k.public_key()]).unwrap();
    assert_eq!(m.builds.keys().collect::<Vec<_>>(), ["macos", "windows"]);
    assert_eq!(m.builds["macos"].version, "0.1.12.4");

    // A manifest there by a key nobody trusts is not carried over: only what this key vouches for is signed.
    std::fs::write(d.path().join(FILE), key(2).sign(&manifest())).unwrap();
    let mac5 = put("Seedwell-0.1.12.5.zip", b"mac5");
    add(d.path(), &Adding { zip: "Seedwell-0.1.12.5.zip", sha256: &mac5, commit: None, minimum: None }, &k, &[], 9).unwrap();
    let (m, _) = verify(&std::fs::read(d.path().join(FILE)).unwrap(), &[k.public_key()]).unwrap();
    assert_eq!(m.builds.keys().collect::<Vec<_>>(), ["macos"]);
}
