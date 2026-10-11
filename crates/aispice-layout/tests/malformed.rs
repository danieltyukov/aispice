//! Readers of untrusted files never panic: random bytes, and valid files with
//! bytes flipped or cut short, give an error or a result, nothing else. The
//! inputs come from a fixed seed, so a failure reproduces.

use aispice_layout::drc::parse_lyrdb;
use aispice_layout::extract::clean;
use aispice_layout::gds::{self, testwrite};
use aispice_layout::lvs::parse_report;

/// xorshift64*: small, deterministic, good enough to shake a parser.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// Flip, insert, delete or truncate a few bytes.
fn mutate(rng: &mut Rng, base: &[u8]) -> Vec<u8> {
    let mut v = base.to_vec();
    for _ in 0..1 + rng.below(8) {
        if v.is_empty() {
            break;
        }
        let at = rng.below(v.len());
        match rng.below(4) {
            0 => v[at] = rng.next() as u8,
            1 => v.insert(at, rng.next() as u8),
            2 => {
                v.remove(at);
            }
            _ => v.truncate(at),
        }
    }
    v
}

fn sample_gds() -> Vec<u8> {
    let leaf = testwrite::TestCell {
        name: "leaf".into(),
        rects: vec![(8, 0, 0, 0, 160, 1000), (5, 0, 0, 0, 130, 500)],
        labels: vec![(8, 25, "vout".into(), 80, 500)],
        ..Default::default()
    };
    let top = testwrite::TestCell {
        name: "top".into(),
        refs: vec![("leaf".into(), 0, 0), ("leaf".into(), 500, 0)],
        rects: vec![(10, 0, 0, 0, 200, 200)],
        ..Default::default()
    };
    testwrite::library(&[leaf, top])
}

#[test]
fn the_gds_reader_survives_garbage() {
    let mut rng = Rng(0x5eed_0001);
    let base = sample_gds();
    assert!(gds::parse(&base).is_ok());
    for _ in 0..3000 {
        let n = rng.below(2048);
        let _ = gds::parse(&rng.bytes(n));
        let m = mutate(&mut rng, &base);
        let _ = gds::parse(&m);
        // A real header in front of noise reaches the record loop.
        let mut h = base[..base.len().min(30)].to_vec();
        let n = rng.below(512);
        h.extend(rng.bytes(n));
        let _ = gds::parse(&h);
    }
}

const LYRDB: &str = r#"<?xml version='1.0' encoding='utf-8'?>
<report-database><top-cell>TOP</top-cell><categories><category><name>M1.b</name><description>space</description></category></categories>
<items><item><category>'M1.b'</category><cell>TOP</cell><values><value>edge-pair: (1,2;1,3)|(1.1,3;1.1,2)</value></values></item>
<item><category>'M1.b'</category><cell>TOP</cell><values><value>polygon: (0,0;0,1;1,1;1,0)</value></values></item></items></report-database>"#;

#[test]
fn the_drc_report_reader_survives_garbage() {
    let mut rng = Rng(0x5eed_0002);
    assert!(parse_lyrdb(LYRDB).is_ok());
    for _ in 0..3000 {
        let m = mutate(&mut rng, LYRDB.as_bytes());
        let _ = parse_lyrdb(&String::from_utf8_lossy(&m));
        let n = rng.below(1024);
        let _ = parse_lyrdb(&String::from_utf8_lossy(&rng.bytes(n)));
    }
    // Deep nesting is refused by the XML reader, not by the stack.
    let deep = format!(
        "<report-database>{}{}</report-database>",
        "<a>".repeat(5000),
        "</a>".repeat(5000)
    );
    let _ = parse_lyrdb(&deep);
}

const LVS_JSON: &str = r#"{"circuits":[{"layout":"ota","schematic":"OTA","status":"mismatch","nets":[{"layout":"$5","schematic":"TAIL","status":"mismatch"}],"devices":[{"layout":null,"schematic":"M5","status":"nomatch"}],"pins":[]}],"log":["x"]}"#;

#[test]
fn the_lvs_report_reader_survives_garbage() {
    let mut rng = Rng(0x5eed_0003);
    assert!(parse_report(LVS_JSON).is_ok());
    for _ in 0..3000 {
        let m = mutate(&mut rng, LVS_JSON.as_bytes());
        let _ = parse_report(&String::from_utf8_lossy(&m));
    }
    let deep = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
    let _ = parse_report(&deep);
}

const MAGIC: &str = "* NGSPICE file created from ota5t.ext\n.subckt ota5t 0 inm out\nX0 0 inm a_1# 0 sg13_lv_nmos ad=0.19p w=1u l=1u\nC0 inm out 0.2f\nR0 out out.t1 3.2\n.ends\n";

#[test]
fn the_extracted_netlist_reader_survives_garbage() {
    let mut rng = Rng(0x5eed_0004);
    assert!(clean(MAGIC, "ota5t", None).is_ok());
    for _ in 0..3000 {
        let m = mutate(&mut rng, MAGIC.as_bytes());
        let text = String::from_utf8_lossy(&m);
        let _ = clean(&text, "ota5t", None);
        let _ = clean(&text, "ota5t", Some(("a_1#", "0")));
    }
}
