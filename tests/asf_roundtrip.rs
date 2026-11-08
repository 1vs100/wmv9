mod common;

use wmv9::asf::Asf;

#[test]
fn every_sample_round_trips_byte_exact() {
    for path in common::sample_files() {
        let original = std::fs::read(&path).unwrap();
        let asf = Asf::parse(&original)
            .unwrap_or_else(|e| panic!("{}: parse failed: {e}", path.display()));
        let written = asf.to_bytes().unwrap();
        if let Some(at) = original.iter().zip(&written).position(|(a, b)| a != b) {
            panic!("{}: first difference at byte {at:#x}", path.display());
        }
        assert_eq!(written.len(), original.len(), "{}", path.display());
    }
}
