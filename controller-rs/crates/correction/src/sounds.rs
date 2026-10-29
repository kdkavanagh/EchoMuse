//! Speech sounds: ARPAbet phonemes reduced to segments, and how alike two sequences of
//! segments sound.
//!
//! A segment is one articulatory sound. ARPAbet's monophthongs and consonants are one
//! segment each; its five diphthongs are two (their onset and their glide), so "bite" and
//! "bat it" share most of their sound. Stress is dropped, and AX counts as AH.
//!
//! How far apart two segments are comes from a small articulatory description of each:
//! for a vowel its height, frontness, rounding and r-colouring; for a consonant where in
//! the mouth it is made, how, and whether it is voiced. The weights below are this crate's
//! own choice, made so that the confusions speech recognizers actually make cost little
//! (T/D, S/SH, TH/F, IY/IH) and unrelated sounds cost as much as deleting one and inserting
//! the other.

use std::fmt;

/// One speech sound.
#[allow(missing_docs)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum Segment {
    AA,
    AE,
    AH,
    AO,
    EH,
    ER,
    IH,
    IY,
    UH,
    UW,
    B,
    CH,
    D,
    DH,
    F,
    G,
    HH,
    JH,
    K,
    L,
    M,
    N,
    NG,
    P,
    R,
    S,
    SH,
    T,
    TH,
    V,
    W,
    Y,
    Z,
    ZH,
}

/// How many segments there are.
pub const SEGMENTS: usize = 34;

const ALL: [Segment; SEGMENTS] = {
    use Segment::*;
    [
        AA, AE, AH, AO, EH, ER, IH, IY, UH, UW, B, CH, D, DH, F, G, HH, JH, K, L, M, N, NG, P, R,
        S, SH, T, TH, V, W, Y, Z, ZH,
    ]
};

impl Segment {
    /// The segment's ARPAbet name.
    pub fn name(self) -> &'static str {
        use Segment::*;
        match self {
            AA => "AA",
            AE => "AE",
            AH => "AH",
            AO => "AO",
            EH => "EH",
            ER => "ER",
            IH => "IH",
            IY => "IY",
            UH => "UH",
            UW => "UW",
            B => "B",
            CH => "CH",
            D => "D",
            DH => "DH",
            F => "F",
            G => "G",
            HH => "HH",
            JH => "JH",
            K => "K",
            L => "L",
            M => "M",
            N => "N",
            NG => "NG",
            P => "P",
            R => "R",
            S => "S",
            SH => "SH",
            T => "T",
            TH => "TH",
            V => "V",
            W => "W",
            Y => "Y",
            Z => "Z",
            ZH => "ZH",
        }
    }

    /// The segments one ARPAbet symbol stands for, its stress digit ignored: one for a
    /// monophthong or consonant, two for a diphthong. `None` for anything else.
    pub fn from_arpabet(symbol: &str) -> Option<&'static [Segment]> {
        use Segment::*;
        let base = symbol.trim_end_matches(['0', '1', '2']);
        if symbol.len() - base.len() > 1 {
            return None;
        }
        let one = |s: Segment| -> &'static [Segment] {
            // A static slice of every segment, so a single segment needs no allocation.
            let i = s as usize;
            &ALL[i..i + 1]
        };
        Some(match base.to_ascii_uppercase().as_str() {
            "AA" => one(AA),
            "AE" => one(AE),
            "AH" | "AX" => one(AH),
            "AO" => one(AO),
            "EH" => one(EH),
            "ER" => one(ER),
            "IH" => one(IH),
            "IY" => one(IY),
            "UH" => one(UH),
            "UW" => one(UW),
            "AW" => &[AA, UH],
            "AY" => &[AA, IH],
            "EY" => &[EH, IH],
            "OW" => &[AO, UH],
            "OY" => &[AO, IH],
            "B" => one(B),
            "CH" => one(CH),
            "D" => one(D),
            "DH" => one(DH),
            "F" => one(F),
            "G" => one(G),
            "HH" => one(HH),
            "JH" => one(JH),
            "K" => one(K),
            "L" => one(L),
            "M" => one(M),
            "N" => one(N),
            "NG" => one(NG),
            "P" => one(P),
            "R" => one(R),
            "S" => one(S),
            "SH" => one(SH),
            "T" => one(T),
            "TH" => one(TH),
            "V" => one(V),
            "W" => one(W),
            "Y" => one(Y),
            "Z" => one(Z),
            "ZH" => one(ZH),
            _ => return None,
        })
    }

    fn is_vowel(self) -> bool {
        (self as u8) <= (Segment::UW as u8)
    }
}

/// A pronunciation: a sequence of segments. Never empty.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Sounds(Box<[Segment]>);

impl Sounds {
    /// The most segments a pronunciation may have; longer text is not compared by sound.
    pub const MAX_SEGMENTS: usize = 96;

    /// A pronunciation from its segments; `None` when there are none or too many.
    pub fn new(segments: Vec<Segment>) -> Option<Self> {
        (!segments.is_empty() && segments.len() <= Self::MAX_SEGMENTS)
            .then(|| Self(segments.into_boxed_slice()))
    }

    /// Parses space-separated ARPAbet, stress digits ignored.
    pub fn parse_arpabet(text: &str) -> Option<Self> {
        let mut segments = Vec::new();
        for symbol in text.split_whitespace() {
            segments.extend_from_slice(Segment::from_arpabet(symbol)?);
        }
        Self::new(segments)
    }

    /// The segments.
    pub fn segments(&self) -> &[Segment] {
        &self.0
    }

    /// How many segments.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Always false: a pronunciation has at least one segment.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// These sounds followed by `other`'s; `None` when the result would be too long.
    pub fn join(&self, other: &Sounds) -> Option<Sounds> {
        let mut segments = Vec::with_capacity(self.len() + other.len());
        segments.extend_from_slice(&self.0);
        segments.extend_from_slice(&other.0);
        Self::new(segments)
    }

    /// The adjacent segment pairs these sounds contain, as a set.
    pub(crate) fn bigrams(&self) -> Bigrams {
        let mut set = Bigrams::default();
        for pair in self.0.windows(2) {
            set.insert(pair[0], pair[1]);
        }
        set
    }
}

impl fmt::Display for Sounds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, segment) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            f.write_str(segment.name())?;
        }
        Ok(())
    }
}

const BIGRAM_WORDS: usize = (SEGMENTS * SEGMENTS).div_ceil(64);

/// A set of adjacent segment pairs, used to choose which rows are worth comparing in full.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Bigrams([u64; BIGRAM_WORDS]);

impl Bigrams {
    fn insert(&mut self, a: Segment, b: Segment) {
        let bit = a as usize * SEGMENTS + b as usize;
        self.0[bit / 64] |= 1 << (bit % 64);
    }

    /// How many pairs both sets hold.
    pub fn shared(&self, other: &Bigrams) -> u32 {
        self.0
            .iter()
            .zip(other.0.iter())
            .map(|(a, b)| (a & b).count_ones())
            .sum()
    }
}

#[derive(Clone, Copy)]
enum Manner {
    Stop,
    Affricate,
    Fricative,
    Nasal,
    Liquid,
    Glide,
}

#[derive(Clone, Copy)]
enum Description {
    /// Height 0 (close) to 3 (open); frontness 0 (front) to 2 (back).
    Vowel {
        height: u8,
        back: u8,
        round: bool,
        rhotic: bool,
    },
    /// Place 0 (both lips) to 7 (glottis).
    Consonant {
        place: u8,
        manner: Manner,
        voiced: bool,
    },
}

fn describe(segment: Segment) -> Description {
    use Description::{Consonant as C, Vowel as V};
    use Manner::*;
    use Segment::*;
    let v = |height, back, round, rhotic| V {
        height,
        back,
        round,
        rhotic,
    };
    let c = |place, manner, voiced| C {
        place,
        manner,
        voiced,
    };
    match segment {
        IY => v(0, 0, false, false),
        IH => v(1, 0, false, false),
        EH => v(2, 0, false, false),
        AE => v(3, 0, false, false),
        AH => v(2, 1, false, false),
        ER => v(2, 1, false, true),
        AA => v(3, 2, false, false),
        AO => v(2, 2, true, false),
        UH => v(1, 2, true, false),
        UW => v(0, 2, true, false),
        P => c(0, Stop, false),
        B => c(0, Stop, true),
        M => c(0, Nasal, true),
        W => c(0, Glide, true),
        F => c(1, Fricative, false),
        V => c(1, Fricative, true),
        TH => c(2, Fricative, false),
        DH => c(2, Fricative, true),
        T => c(3, Stop, false),
        D => c(3, Stop, true),
        N => c(3, Nasal, true),
        S => c(3, Fricative, false),
        Z => c(3, Fricative, true),
        L => c(3, Liquid, true),
        R => c(4, Liquid, true),
        CH => c(4, Affricate, false),
        JH => c(4, Affricate, true),
        SH => c(4, Fricative, false),
        ZH => c(4, Fricative, true),
        Y => c(5, Glide, true),
        K => c(6, Stop, false),
        G => c(6, Stop, true),
        NG => c(6, Nasal, true),
        HH => c(7, Fricative, false),
    }
}

fn manner_cost(a: Manner, b: Manner) -> f32 {
    use Manner::*;
    match (a, b) {
        (Stop, Stop)
        | (Affricate, Affricate)
        | (Fricative, Fricative)
        | (Nasal, Nasal)
        | (Liquid, Liquid)
        | (Glide, Glide) => 0.0,
        (Stop, Affricate) | (Affricate, Stop) => 0.3,
        (Affricate, Fricative) | (Fricative, Affricate) => 0.3,
        (Stop, Fricative) | (Fricative, Stop) => 0.5,
        (Liquid, Glide) | (Glide, Liquid) => 0.4,
        (Nasal, Stop) | (Stop, Nasal) => 0.6,
        (Nasal, Liquid) | (Liquid, Nasal) => 0.7,
        _ => 1.0,
    }
}

/// The cost of deleting or inserting one segment.
pub(crate) const INDEL: f32 = 1.0;
/// The cost of deleting or inserting a weak sound (AH, HH), which recognizers drop and add.
const WEAK_INDEL: f32 = 0.6;
/// The most a substitution costs: as much as deleting one segment and inserting another.
const MAX_SUBSTITUTION: f32 = 2.0;

fn substitution(a: Segment, b: Segment) -> f32 {
    use Description::*;
    if a == b {
        return 0.0;
    }
    let cost = match (describe(a), describe(b)) {
        (
            Vowel {
                height: h1,
                back: b1,
                round: r1,
                rhotic: x1,
            },
            Vowel {
                height: h2,
                back: b2,
                round: r2,
                rhotic: x2,
            },
        ) => {
            f32::from(h1.abs_diff(h2)) / 3.0 * 0.8
                + f32::from(b1.abs_diff(b2)) / 2.0 * 0.6
                + if r1 != r2 { 0.2 } else { 0.0 }
                + if x1 != x2 { 0.6 } else { 0.0 }
        }
        (
            Consonant {
                place: p1,
                manner: m1,
                voiced: v1,
            },
            Consonant {
                place: p2,
                manner: m2,
                voiced: v2,
            },
        ) => {
            f32::from(p1.abs_diff(p2)) / 7.0 * 1.2
                + manner_cost(m1, m2)
                + if v1 != v2 { 0.3 } else { 0.0 }
        }
        _ => {
            use Segment::*;
            let (vowel, consonant) = if a.is_vowel() { (a, b) } else { (b, a) };
            match (vowel, consonant) {
                (ER, R) | (IY, Y) | (UW, W) => 0.5,
                (_, L) => 1.3,
                (_, HH) => 1.2,
                _ => 1.6,
            }
        }
    };
    cost.min(MAX_SUBSTITUTION)
}

fn indel(segment: Segment) -> f32 {
    match segment {
        Segment::AH | Segment::HH => WEAK_INDEL,
        _ => INDEL,
    }
}

/// Substitution costs between every pair of segments, computed once.
static COSTS: std::sync::LazyLock<[[f32; SEGMENTS]; SEGMENTS]> = std::sync::LazyLock::new(|| {
    let mut table = [[0.0; SEGMENTS]; SEGMENTS];
    for a in ALL {
        for b in ALL {
            table[a as usize][b as usize] = substitution(a, b);
        }
    }
    table
});

/// Counts the work a comparison did, so a turn's cost can be bounded.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Work {
    /// Edit-distance cells computed.
    pub cells: u64,
    /// Table rows considered.
    pub rows: u64,
}

/// How alike two pronunciations sound, 0 to 1: one minus their weighted edit distance over
/// the longer one's length. `None` when it is certainly below `min`; the comparison then
/// stops early. `work` counts the cells computed.
pub fn similarity(a: &Sounds, b: &Sounds, min: f32, work: &mut Work) -> Option<f32> {
    let (a, b) = (a.segments(), b.segments());
    let longer = a.len().max(b.len()) as f32;
    // Distance can be at most this for similarity to reach `min`.
    let limit = (1.0 - min) * longer;
    let costs = &*COSTS;
    let mut previous: Vec<f32> = Vec::with_capacity(b.len() + 1);
    previous.push(0.0);
    for (j, &sb) in b.iter().enumerate() {
        previous.push(previous[j] + indel(sb));
    }
    let mut current = vec![0.0f32; b.len() + 1];
    for &sa in a {
        current[0] = previous[0] + indel(sa);
        let mut row_min = current[0];
        let row = &costs[sa as usize];
        for (j, &sb) in b.iter().enumerate() {
            let value = (previous[j] + row[sb as usize])
                .min(previous[j + 1] + indel(sa))
                .min(current[j] + indel(sb));
            current[j + 1] = value;
            row_min = row_min.min(value);
        }
        work.cells += b.len() as u64;
        if row_min > limit + 1e-6 {
            return None;
        }
        std::mem::swap(&mut previous, &mut current);
    }
    let distance = previous[b.len()];
    let score = (1.0 - distance / longer).max(0.0);
    (score + 1e-6 >= min).then_some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str) -> Sounds {
        Sounds::parse_arpabet(text).unwrap()
    }

    fn sim(a: &str, b: &str) -> f32 {
        similarity(&s(a), &s(b), 0.0, &mut Work::default()).unwrap()
    }

    #[test]
    fn diphthongs_are_two_segments_and_stress_is_ignored() {
        assert_eq!(s("AY1").segments(), &[Segment::AA, Segment::IH]);
        assert_eq!(s("AX0 AH1"), s("AH AH"));
        assert!(Sounds::parse_arpabet("QQ").is_none());
        assert!(Sounds::parse_arpabet("AH12").is_none());
    }

    #[test]
    fn substitution_costs_are_symmetric_and_bounded() {
        for a in ALL {
            for b in ALL {
                let ab = substitution(a, b);
                assert_eq!(ab, substitution(b, a));
                assert!((0.0..=MAX_SUBSTITUTION).contains(&ab));
                assert_eq!(ab == 0.0, a == b, "{a:?} {b:?}");
            }
        }
        // Close confusions cost little; unrelated sounds cost a lot.
        assert!(substitution(Segment::T, Segment::D) < 0.4);
        assert!(substitution(Segment::TH, Segment::F) < 0.4);
        assert!(substitution(Segment::IY, Segment::IH) < 0.4);
        assert!(substitution(Segment::AA, Segment::K) > 1.5);
    }

    #[test]
    fn similar_words_score_high_and_different_ones_low() {
        // plex / flex
        assert!(sim("P L EH K S", "F L EH K S") > 0.85);
        // play / plate
        assert!(sim("P L EY", "P L EY T") >= 0.8);
        // kitchen / bedroom
        assert!(sim("K IH CH AH N", "B EH D R UW M") < 0.5);
        assert_eq!(sim("K IH CH AH N", "K IH CH AH N"), 1.0);
    }

    #[test]
    fn a_hopeless_comparison_stops_early() {
        let mut work = Work::default();
        let long = s("K IH CH AH N K IH CH AH N K IH CH AH N");
        let other = s("B EH D R UW M B EH D R UW M B EH D R UW M");
        assert!(similarity(&long, &other, 0.9, &mut work).is_none());
        assert!(work.cells < (long.len() * other.len()) as u64);
    }
}
