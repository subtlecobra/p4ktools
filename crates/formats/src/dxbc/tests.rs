use super::*;
use std::path::PathBuf;

fn corpus_dir() -> Option<PathBuf> {
    let p = match std::env::var("DXBC_CORPUS") {
        Ok(p) => PathBuf::from(p),
        Err(_) => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/shader-samples/dxbc"),
    };
    p.is_dir().then_some(p)
}

/// Runs `decode` over every `*.dxbc` file of the extracted sample and prints the figures.
/// Skips when the directory is missing. `DXBC_LIMIT=n` keeps every k-th file so about n are decoded; `DXBC_DUMP=name` prints
/// the decoded text of the file with that stem.
#[test]
fn corpus_decodes_without_failures() {
    use rayon::prelude::*;
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    let Some(dir) = corpus_dir() else {
        eprintln!("dxbc corpus not found, skipping");
        return;
    };
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().map(|e| e == "dxbc").unwrap_or(false)).collect();
    files.sort();
    if let Ok(n) = std::env::var("DXBC_LIMIT").map(|v| v.parse::<usize>().unwrap_or(5000)) {
        let step = (files.len() / n.max(1)).max(1);
        files = files.into_iter().step_by(step).collect();
    }
    if let Ok(stem) = std::env::var("DXBC_DUMP") {
        let p = dir.join(format!("{}.dxbc", stem));
        let s = decode(&std::fs::read(p).unwrap()).unwrap();
        println!("{}\n=====\n{}\n=====\nwarnings: {:?}", s.disassembly, s.hlsl, s.warnings);
        return;
    }
    if let Ok(list) = std::env::var("DXBC_FIND") {
        // print the smallest sample files containing each mnemonic (development aid)
        let wanted: Vec<String> = list.split(',').map(|s| s.to_string()).collect();
        let found = std::sync::Mutex::new(std::collections::BTreeMap::<String, Vec<(usize, String)>>::new());
        files.par_iter().for_each(|f| {
            let d = std::fs::read(f).unwrap();
            if let Ok(s) = decode(&d) {
                for w in &wanted {
                    if s.disassembly.lines().any(|l| l.trim_start().starts_with(w.as_str())) {
                        let mut g = found.lock().unwrap();
                        let v = g.entry(w.clone()).or_default();
                        v.push((d.len(), f.file_stem().unwrap().to_string_lossy().into_owned()));
                        v.sort();
                        v.truncate(3);
                    }
                }
            }
        });
        println!("FIND {:?}", found.lock().unwrap());
        return;
    }
    let (fail, unk, warn, panic, ok, bad_instr) = (AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0));
    let by_stage = std::sync::Mutex::new(std::collections::BTreeMap::<String, usize>::new());
    let examples = std::sync::Mutex::new(Vec::<String>::new());
    let ops = std::sync::Mutex::new(std::collections::BTreeMap::<String, usize>::new());
    files.par_iter().for_each(|f| {
        let d = std::fs::read(f).unwrap();
        match decode(&d) {
            Ok(s) => {
                ok.fetch_add(1, Relaxed);
                {
                    let mut local = std::collections::BTreeMap::<String, usize>::new();
                    let body = s.disassembly.rsplit("\n// ").next().unwrap_or("");
                    for l in body.lines().skip(1) {
                        let l = l.trim_start();
                        if l.is_empty() || l.starts_with("//") || l.starts_with('{') || l.starts_with('}') {
                            continue;
                        }
                        let w = l.split(|c: char| c == ' ' || c == '(').next().unwrap_or("");
                        let w = w.split("_sat").next().unwrap_or(w);
                        *local.entry(w.to_string()).or_default() += 1;
                    }
                    let mut g = ops.lock().unwrap();
                    for (k, v) in local {
                        *g.entry(k).or_default() += v;
                    }
                }
                *by_stage.lock().unwrap().entry(s.stage.clone()).or_default() += 1;
                if !s.warnings.is_empty() {
                    warn.fetch_add(1, Relaxed);
                    let mut ex = examples.lock().unwrap();
                    if ex.len() < 20 {
                        ex.push(format!("{}: {:?}", f.display(), &s.warnings[..s.warnings.len().min(3)]));
                    }
                }
                if s.warnings.iter().any(|w| w.contains("unknown opcode")) {
                    unk.fetch_add(1, Relaxed);
                }
                if s.warnings.iter().any(|w| w.contains("printed raw") || w.contains("bad length")) {
                    bad_instr.fetch_add(1, Relaxed);
                }
            }
            Err(e) => {
                if e.contains("panic") {
                    panic.fetch_add(1, Relaxed);
                }
                fail.fetch_add(1, Relaxed);
                let mut ex = examples.lock().unwrap();
                if ex.len() < 20 {
                    ex.push(format!("{}: ERR {}", f.display(), e));
                }
            }
        }
    });
    println!(
        "dxbc corpus: files={} ok={} parse_failures={} panics={} with_unknown_opcode={} with_bad_instruction={} with_warnings={}",
        files.len(),
        ok.load(Relaxed),
        fail.load(Relaxed),
        panic.load(Relaxed),
        unk.load(Relaxed),
        bad_instr.load(Relaxed),
        warn.load(Relaxed)
    );
    println!("by stage: {:?}", by_stage.lock().unwrap());
    println!("mnemonics: {:?}", ops.lock().unwrap());
    for e in examples.lock().unwrap().iter() {
        println!("  {}", e);
    }
    assert_eq!(fail.load(Relaxed), 0);
    assert_eq!(unk.load(Relaxed), 0);
}

#[test]
fn rejects_garbage_without_panicking() {
    assert!(decode(b"").is_err());
    assert!(decode(b"DXBC").is_err());
    let mut v = b"DXBC".to_vec();
    v.extend_from_slice(&[0u8; 24]);
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&0xffff_ffffu32.to_le_bytes());
    assert!(decode(&v).is_err());
    // random-ish bytes after a valid header never panic
    for seed in 0..200u32 {
        let mut d = b"DXBC".to_vec();
        d.extend_from_slice(&[0u8; 20]);
        d.extend_from_slice(&1u32.to_le_bytes());
        d.extend_from_slice(&200u32.to_le_bytes());
        d.extend_from_slice(&1u32.to_le_bytes());
        d.extend_from_slice(&36u32.to_le_bytes());
        let mut x = seed.wrapping_mul(2654435761).wrapping_add(1);
        d.extend_from_slice(b"SHEX");
        d.extend_from_slice(&120u32.to_le_bytes());
        for _ in 0..30 {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            d.extend_from_slice(&x.to_le_bytes());
        }
        let _ = decode(&d);
    }
}
