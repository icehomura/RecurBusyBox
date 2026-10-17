// Throwaway fixture generator for manual verification of `extract`.
// Builds a tar+gzip archive (two good files + one `../evil` traversal entry)
// using the same pure-Rust crates the applet depends on.
use flate2::write::GzEncoder;
use flate2::Compression;

fn add(ar: &mut tar::Builder<GzEncoder<std::fs::File>>, path: &str, data: &mut &[u8]) {
    let mut h = tar::Header::new_ustar();
    h.set_entry_type(tar::EntryType::Regular);
    h.set_path(path).unwrap();
    h.set_mode(0o644);
    h.set_size(data.len() as u64);
    h.set_mtime(1_700_000_000);
    ar.append(&h, data).unwrap();
}

fn main() {
    let out = std::env::args()
        .nth(1)
        .expect("usage: make_tar_gz <OUT.tar.gz>");
    let file = std::fs::File::create(&out).unwrap();
    let gz = GzEncoder::new(file, Compression::default());
    let mut ar = tar::Builder::new(gz);

    let a = b"hello from alpha\n".to_vec();
    add(&mut ar, "dir/alpha.txt", &mut a.as_slice());
    let b = b"bravo payload\nsecond line\n".to_vec();
    add(&mut ar, "dir/bravo.txt", &mut b.as_slice());
    // A deliberate traversal entry.
    let e = b"I should be refused\n".to_vec();
    add(&mut ar, "../evil", &mut e.as_slice());

    ar.into_inner().unwrap().finish().unwrap();
    println!("wrote {out}");
}
