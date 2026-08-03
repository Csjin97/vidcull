#![allow(clippy::too_many_lines)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::cast_precision_loss)]

use std::fmt;

use vidcull_core::types::FileId;
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};
use vidcull_matcher::partial::{ClipAlignment, partial_clip_params, plan_partial_clips};

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn flip_low_bits(h: u64, n: u32) -> u64 {
    if n == 0 {
        return h;
    }
    let mask = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
    h ^ mask
}

fn scene(ts: u64, phash: u64) -> SceneHash {
    SceneHash {
        timestamp_ms: ts,
        phash,
    }
}

fn unrelated_seq(seed: u64, n: usize) -> Vec<SceneHash> {
    let mut state = seed;
    (0..n)
        .map(|i| scene(i as u64 * 2500, splitmix64(&mut state) | 1))
        .collect()
}

fn dur_ms(n: usize) -> u64 {
    (n as u64) * 2500
}

fn shared_intro_pair(
    seed_shared: u64,
    seed_a: u64,
    seed_b: u64,
    k: usize,
    a_len: usize,
    b_len: usize,
) -> (Tier2Fingerprint, Tier2Fingerprint) {
    assert!(k <= a_len && k <= b_len);
    let mut state_shared = seed_shared;
    let shared: Vec<u64> = (0..k).map(|_| splitmix64(&mut state_shared) | 1).collect();

    let mut state_a = seed_a;
    let mut a_scenes: Vec<SceneHash> = (0..k).map(|i| scene(i as u64 * 2500, shared[i])).collect();
    a_scenes.extend((k..a_len).map(|i| scene(i as u64 * 2500, splitmix64(&mut state_a) | 1)));

    let mut state_b = seed_b;
    let mut b_scenes: Vec<SceneHash> = (0..k).map(|i| scene(i as u64 * 2500, shared[i])).collect();
    b_scenes.extend((k..b_len).map(|i| scene(i as u64 * 2500, splitmix64(&mut state_b) | 1)));

    (
        Tier2Fingerprint { scenes: a_scenes },
        Tier2Fingerprint { scenes: b_scenes },
    )
}

fn shared_outro_pair(
    seed_shared: u64,
    seed_a: u64,
    seed_b: u64,
    k: usize,
    a_len: usize,
    b_len: usize,
) -> (Tier2Fingerprint, Tier2Fingerprint) {
    assert!(k <= a_len && k <= b_len);
    let mut state_shared = seed_shared;
    let shared: Vec<u64> = (0..k).map(|_| splitmix64(&mut state_shared) | 1).collect();

    let mut state_a = seed_a;
    let mut a_scenes: Vec<SceneHash> = (0..(a_len - k))
        .map(|i| scene(i as u64 * 2500, splitmix64(&mut state_a) | 1))
        .collect();
    a_scenes.extend((0..k).map(|i| scene((a_len - k + i) as u64 * 2500, shared[i])));

    let mut state_b = seed_b;
    let mut b_scenes: Vec<SceneHash> = (0..(b_len - k))
        .map(|i| scene(i as u64 * 2500, splitmix64(&mut state_b) | 1))
        .collect();
    b_scenes.extend((0..k).map(|i| scene((b_len - k + i) as u64 * 2500, shared[i])));

    (
        Tier2Fingerprint { scenes: a_scenes },
        Tier2Fingerprint { scenes: b_scenes },
    )
}

struct IntroOutroSpec {
    seed_head: u64,
    seed_tail: u64,
    seed_a: u64,
    seed_b: u64,
    k_head: usize,
    k_tail: usize,
    a_len: usize,
    b_len: usize,
}

fn shared_intro_and_outro_pair(spec: &IntroOutroSpec) -> (Tier2Fingerprint, Tier2Fingerprint) {
    let &IntroOutroSpec {
        seed_head,
        seed_tail,
        seed_a,
        seed_b,
        k_head,
        k_tail,
        a_len,
        b_len,
    } = spec;
    assert!(k_head + k_tail <= a_len && k_head + k_tail <= b_len);
    let mut state_head = seed_head;
    let head: Vec<u64> = (0..k_head)
        .map(|_| splitmix64(&mut state_head) | 1)
        .collect();
    let mut state_tail = seed_tail;
    let tail: Vec<u64> = (0..k_tail)
        .map(|_| splitmix64(&mut state_tail) | 1)
        .collect();

    let build = |seed_body: u64, len: usize| -> Vec<SceneHash> {
        let mut state_body = seed_body;
        let mut scenes: Vec<SceneHash> = (0..k_head)
            .map(|i| scene(i as u64 * 2500, head[i]))
            .collect();
        scenes.extend(
            (k_head..(len - k_tail))
                .map(|i| scene(i as u64 * 2500, splitmix64(&mut state_body) | 1)),
        );
        scenes.extend((0..k_tail).map(|i| scene((len - k_tail + i) as u64 * 2500, tail[i])));
        scenes
    };

    (
        Tier2Fingerprint {
            scenes: build(seed_a, a_len),
        },
        Tier2Fingerprint {
            scenes: build(seed_b, b_len),
        },
    )
}

fn clip_embedded_at(
    source: &Tier2Fingerprint,
    at: usize,
    len: usize,
    perturb: u32,
) -> Tier2Fingerprint {
    let scenes = source.scenes[at..at + len]
        .iter()
        .enumerate()
        .map(|(i, s)| scene(i as u64 * 2500, flip_low_bits(s.phash, perturb)))
        .collect();
    Tier2Fingerprint { scenes }
}

fn group7_reframe(
    seed_shared: u64,
    seed_clip_body: u64,
    seed_source_body: u64,
    clip_len: usize,
    source_len: usize,
    aligned_at: usize,
    source_offset: usize,
) -> (Tier2Fingerprint, Tier2Fingerprint) {
    assert!(aligned_at + 3 <= clip_len);
    assert!(source_offset + 3 <= source_len);
    let mut state_shared = seed_shared;
    let shared: [u64; 3] = [
        splitmix64(&mut state_shared) | 1,
        splitmix64(&mut state_shared) | 1,
        splitmix64(&mut state_shared) | 1,
    ];

    let mut state_clip = seed_clip_body;
    let mut clip_scenes: Vec<SceneHash> = (0..clip_len)
        .map(|i| scene(i as u64 * 2500, splitmix64(&mut state_clip) | 1))
        .collect();
    for (i, &h) in shared.iter().enumerate() {
        clip_scenes[aligned_at + i] = scene((aligned_at + i) as u64 * 2500, h);
    }

    let mut state_src = seed_source_body;
    let mut src_scenes: Vec<SceneHash> = (0..source_len)
        .map(|i| scene(i as u64 * 2500, splitmix64(&mut state_src) | 1))
        .collect();
    for (i, &h) in shared.iter().enumerate() {
        src_scenes[source_offset + i] = scene((source_offset + i) as u64 * 2500, h);
    }

    (
        Tier2Fingerprint {
            scenes: clip_scenes,
        },
        Tier2Fingerprint { scenes: src_scenes },
    )
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pos {
    Head,
    Mid,
    Tail,
}

impl fmt::Display for Pos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Pos::Head => "head",
            Pos::Mid => "mid",
            Pos::Tail => "tail",
        };
        write!(f, "{s}")
    }
}

fn position_in(mid_ms: u64, dur_ms: u64) -> Pos {
    if dur_ms == 0 {
        return Pos::Mid;
    }
    let third = dur_ms / 3;
    if mid_ms <= third {
        Pos::Head
    } else if mid_ms >= dur_ms - third {
        Pos::Tail
    } else {
        Pos::Mid
    }
}

struct Metrics {
    clip_span_ratio_x1000: u64,
    source_span_ratio_x1000: u64,
    clip_pos: Pos,
    source_pos: Pos,
}

fn compute_metrics(a: &ClipAlignment, clip_dur_ms: u64, source_dur_ms: u64) -> Metrics {
    let clip_span = a.clip_end_ms.saturating_sub(a.clip_start_ms);
    let source_span = a.end_ms.saturating_sub(a.start_ms);
    let clip_mid = a.clip_start_ms + clip_span / 2;
    let source_mid = a.start_ms + source_span / 2;
    Metrics {
        clip_span_ratio_x1000: if clip_dur_ms == 0 {
            0
        } else {
            clip_span.saturating_mul(1000) / clip_dur_ms
        },
        source_span_ratio_x1000: if source_dur_ms == 0 {
            0
        } else {
            source_span.saturating_mul(1000) / source_dur_ms
        },
        clip_pos: position_in(clip_mid, clip_dur_ms),
        source_pos: position_in(source_mid, source_dur_ms),
    }
}

fn is_intro_outro(m: &Metrics, span_pct_x1000: u64) -> bool {
    let short_both =
        m.clip_span_ratio_x1000 <= span_pct_x1000 && m.source_span_ratio_x1000 <= span_pct_x1000;
    let localized_both = m.clip_pos != Pos::Mid && m.source_pos != Pos::Mid;
    short_both && localized_both
}

struct Case {
    label: &'static str,
    class: &'static str,
    clip_dur_ms: u64,
    source_dur_ms: u64,
    fp_clip: Tier2Fingerprint,
    fp_source: Tier2Fingerprint,
}

fn run_case(case: &Case) -> Option<(ClipAlignment, Metrics)> {
    let corpus = vec![
        (FileId(1), case.fp_source.clone()),
        (FileId(2), case.fp_clip.clone()),
    ];
    let plan = plan_partial_clips(corpus, partial_clip_params());
    let m = plan
        .matches
        .into_iter()
        .find(|m| m.clip == FileId(2) && m.alignment.source == FileId(1))?;
    let metrics = compute_metrics(&m.alignment, case.clip_dur_ms, case.source_dur_ms);
    Some((m.alignment, metrics))
}

#[test]
fn gate2_intro_outro_vs_reframe_separation() {
    let mut cases: Vec<Case> = Vec::new();

    for (k, a_len, b_len) in [(3usize, 400usize, 24usize), (6, 800, 60), (12, 40, 30)] {
        let (fp_a, fp_b) =
            shared_intro_pair(0xA000_0001, 0xA100_0001, 0xA200_0001, k, a_len, b_len);
        cases.push(Case {
            label: "A-shared-intro",
            class: "A",
            clip_dur_ms: dur_ms(b_len),
            source_dur_ms: dur_ms(a_len),
            fp_clip: fp_b,
            fp_source: fp_a,
        });
    }

    for (k, a_len, b_len) in [(3usize, 400usize, 24usize), (6, 800, 60), (12, 40, 30)] {
        let (fp_a, fp_b) =
            shared_outro_pair(0xB000_0001, 0xB100_0001, 0xB200_0001, k, a_len, b_len);
        cases.push(Case {
            label: "A-shared-outro",
            class: "A",
            clip_dur_ms: dur_ms(b_len),
            source_dur_ms: dur_ms(a_len),
            fp_clip: fp_b,
            fp_source: fp_a,
        });
    }

    for (k_head, k_tail, a_len, b_len) in [(3usize, 3usize, 400usize, 24usize), (5, 5, 60, 40)] {
        let (fp_a, fp_b) = shared_intro_and_outro_pair(&IntroOutroSpec {
            seed_head: 0xC000_0001,
            seed_tail: 0xC100_0001,
            seed_a: 0xC200_0001,
            seed_b: 0xC300_0001,
            k_head,
            k_tail,
            a_len,
            b_len,
        });
        cases.push(Case {
            label: "A-shared-intro-and-outro",
            class: "A",
            clip_dur_ms: dur_ms(b_len),
            source_dur_ms: dur_ms(a_len),
            fp_clip: fp_b,
            fp_source: fp_a,
        });
    }

    {
        let source = Tier2Fingerprint {
            scenes: unrelated_seq(0xD000_0001, 200),
        };
        for (label, at) in [
            ("B-embedded-head", 2usize),
            ("B-embedded-mid", 95),
            ("B-embedded-tail", 190),
        ] {
            let clip = clip_embedded_at(&source, at, 8, 3);
            cases.push(Case {
                label,
                class: "B",
                clip_dur_ms: dur_ms(8),
                source_dur_ms: dur_ms(200),
                fp_clip: clip,
                fp_source: source.clone(),
            });
        }
    }

    {
        let source = Tier2Fingerprint {
            scenes: unrelated_seq(0xE000_0001, 300),
        };
        for (label, clip_len, at) in [
            ("B-short-clip-whole-at-head", 24usize, 4usize),
            ("B-short-clip-whole-at-head-2", 40, 0),
        ] {
            let clip = clip_embedded_at(&source, at, clip_len, 3);
            cases.push(Case {
                label,
                class: "B",
                clip_dur_ms: dur_ms(clip_len),
                source_dur_ms: dur_ms(300),
                fp_clip: clip,
                fp_source: source.clone(),
            });
        }
    }

    {
        let (clip, source) = group7_reframe(0xF000_0001, 0xF100_0001, 0xF200_0001, 46, 300, 2, 150);
        cases.push(Case {
            label: "B-group7-localized-head",
            class: "B",
            clip_dur_ms: dur_ms(46),
            source_dur_ms: dur_ms(300),
            fp_clip: clip,
            fp_source: source,
        });
    }
    {
        let (clip, source) = group7_reframe(0xF600_0001, 0xF700_0001, 0xF800_0001, 46, 300, 1, 3);
        cases.push(Case {
            label: "B-group7-both-heads-worst-case",
            class: "B",
            clip_dur_ms: dur_ms(46),
            source_dur_ms: dur_ms(300),
            fp_clip: clip,
            fp_source: source,
        });
    }
    {
        let mut state = 0xF300_0001u64;
        let shared: [u64; 3] = [
            splitmix64(&mut state) | 1,
            splitmix64(&mut state) | 1,
            splitmix64(&mut state) | 1,
        ];
        let clip_len = 46;
        let source_len = 300;
        let mut state_clip = 0xF400_0001u64;
        let mut clip_scenes: Vec<SceneHash> = (0..clip_len)
            .map(|i| scene(i as u64 * 2500, splitmix64(&mut state_clip) | 1))
            .collect();
        let dispersed_at = [2usize, 23, 43];
        let offset_d = 255usize;
        for (i, &at) in dispersed_at.iter().enumerate() {
            clip_scenes[at] = scene(at as u64 * 2500, shared[i]);
        }
        let mut state_src = 0xF500_0001u64;
        let mut src_scenes: Vec<SceneHash> = (0..source_len)
            .map(|i| scene(i as u64 * 2500, splitmix64(&mut state_src) | 1))
            .collect();
        for (i, &at) in dispersed_at.iter().enumerate() {
            let src_at = at + offset_d;
            src_scenes[src_at] = scene(src_at as u64 * 2500, shared[i]);
        }
        cases.push(Case {
            label: "B-group7-dispersed-source-tail",
            class: "B",
            clip_dur_ms: dur_ms(clip_len),
            source_dur_ms: dur_ms(source_len),
            fp_clip: Tier2Fingerprint {
                scenes: clip_scenes,
            },
            fp_source: Tier2Fingerprint { scenes: src_scenes },
        });
    }

    println!(
        "\n{:<32} {:>5} {:>10} {:>10} {:>10} {:>6} {:>6} {:>9} {:>9}",
        "label",
        "class",
        "clip_span%",
        "src_span%",
        "matched",
        "clip_sc",
        "src_sc",
        "clip_pos",
        "src_pos"
    );
    struct Result_ {
        label: &'static str,
        class: &'static str,
        m: Metrics,
        matched: usize,
        clip_scenes: usize,
    }
    let mut results: Vec<Result_> = Vec::new();
    for case in &cases {
        match run_case(case) {
            Some((a, m)) => {
                println!(
                    "{:<32} {:>5} {:>9.1}% {:>9.1}% {:>10} {:>6} {:>6} {:>9} {:>9}",
                    case.label,
                    case.class,
                    m.clip_span_ratio_x1000 as f64 / 10.0,
                    m.source_span_ratio_x1000 as f64 / 10.0,
                    a.matched_scenes,
                    a.clip_scenes,
                    "-",
                    m.clip_pos,
                    m.source_pos,
                );
                results.push(Result_ {
                    label: case.label,
                    class: case.class,
                    m,
                    matched: a.matched_scenes,
                    clip_scenes: a.clip_scenes,
                });
            }
            None => {
                println!(
                    "{:<32} {:>5} {:>10} (NO ALIGNMENT VERIFIED)",
                    case.label, case.class, ""
                );
            }
        }
    }
    println!();

    let mut best: Option<u64> = None;
    for span_pct_x1000 in (0..=1000).step_by(5) {
        let mut all_a_flagged = true;
        let mut any_b_flagged = false;
        for r in &results {
            let flagged = is_intro_outro(&r.m, span_pct_x1000);
            if r.class == "A" && !flagged {
                all_a_flagged = false;
            }
            if r.class == "B" && flagged {
                any_b_flagged = true;
            }
        }
        if all_a_flagged && !any_b_flagged && best.is_none() {
            best = Some(span_pct_x1000);
        }
    }

    let max_span = |r: &Result_| r.m.clip_span_ratio_x1000.max(r.m.source_span_ratio_x1000);
    let a_localized_max_span = results
        .iter()
        .filter(|r| r.class == "A")
        .map(max_span)
        .max()
        .unwrap_or(0);
    let b_localized_min_span = results
        .iter()
        .filter(|r| r.class == "B" && r.m.clip_pos != Pos::Mid && r.m.source_pos != Pos::Mid)
        .map(max_span)
        .min();

    println!("== threshold search ==");
    println!("max class-A span (worst case, x1000)  = {a_localized_max_span}");
    match b_localized_min_span {
        Some(v) => println!("min class-B span among head/tail-localized cases (x1000) = {v}"),
        None => println!("(no class-B case is head/tail-localized on both sides)"),
    }
    let margin_report = match best {
        Some(pct) => format!(
            "SEPARATED: span_pct_x1000={pct} (={:.1}%) satisfies \
             is_intro_outro for all class-A and no class-B case",
            pct as f64 / 10.0
        ),
        None => "NOT SEPARATED: no single span_pct threshold in [0,1000] \
             flags every class-A case without also flagging a class-B case"
            .to_string(),
    };
    println!("{margin_report}");
    println!();

    for r in &results {
        eprintln!(
            "[gate2] {} class={} clip_span={:.1}% src_span={:.1}% clip_pos={} src_pos={} \
             matched={}/{}",
            r.label,
            r.class,
            r.m.clip_span_ratio_x1000 as f64 / 10.0,
            r.m.source_span_ratio_x1000 as f64 / 10.0,
            r.m.clip_pos,
            r.m.source_pos,
            r.matched,
            r.clip_scenes,
        );
    }

    let a_count = cases.iter().filter(|c| c.class == "A").count();
    let a_verified = results.iter().filter(|r| r.class == "A").count();
    assert_eq!(
        a_verified, a_count,
        "every class-A (intro/outro) case must verify an alignment for the \
         predicate to be exercised against it — {a_verified}/{a_count} did"
    );
    let b_count = cases.iter().filter(|c| c.class == "B").count();
    let b_verified = results.iter().filter(|r| r.class == "B").count();
    assert_eq!(
        b_verified, b_count,
        "every class-B (genuine containment) case must verify an alignment — \
         {b_verified}/{b_count} did"
    );
}
