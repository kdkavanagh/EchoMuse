//! Letter-to-sound rules: a pronunciation guessed from spelling, for a word the dictionary
//! does not know and that is not made of words it knows.
//!
//! The rules are English spelling's common patterns, matched longest first from left to
//! right: consonant digraphs ("sh", "ch", "ph", "ck", "ng"), vowel teams ("ee", "oa",
//! "ai"), r-coloured vowels ("ar", "er", "or"), a silent final e that makes the vowel
//! before it long ("mute", "plate"), and soft c before e, i and y. A guess is only ever
//! compared by sound against names and phrases, and every correction it leads to must
//! still pass Home Assistant's matcher, so it needs to be close, not exact.

use crate::sounds::{Segment, Segment::*, Sounds};

fn is_vowel(c: u8) -> bool {
    matches!(c, b'a' | b'e' | b'i' | b'o' | b'u')
}

/// The vowel before a single consonant and a final silent e is long: "mute", "plate".
fn magic_e(letters: &[u8], i: usize) -> bool {
    let n = letters.len();
    // vowel, one consonant, then a final "e" (or "es"/"ed").
    let consonant = i + 1;
    let e = i + 2;
    if e >= n || is_vowel(letters[consonant]) || letters[consonant] == b'r' {
        return false;
    }
    letters[e] == b'e' && (e + 1 == n || (e + 2 == n && matches!(letters[e + 1], b's' | b'd')))
}

/// A pronunciation guessed from `word`'s letters (a–z; anything else is skipped). `None`
/// when it has no letters.
pub fn guess(word: &str) -> Option<Sounds> {
    let letters: Vec<u8> = word
        .bytes()
        .map(|b| b.to_ascii_lowercase())
        .filter(u8::is_ascii_lowercase)
        .collect();
    let n = letters.len();
    let mut out: Vec<Segment> = Vec::new();
    let at = |i: usize| letters.get(i).copied();
    let rest = |i: usize| &letters[i..];
    let mut i = 0;
    while i < n {
        let c = letters[i];
        let next = at(i + 1);
        let after_next = at(i + 2);
        let starts = |pattern: &str| rest(i).starts_with(pattern.as_bytes());
        let ends_after = |len: usize| i + len == n;
        // Longest patterns first.
        let (sounds, used): (&[Segment], usize) = if starts("tion") {
            (&[SH, AH, N], 4)
        } else if starts("sion") {
            (&[ZH, AH, N], 4)
        } else if starts("ture") {
            (&[CH, ER], 4)
        } else if starts("eigh") {
            (&[EH, IH], 4)
        } else if starts("augh") || starts("ough") {
            (&[AO], 4)
        } else if starts("tch") {
            (&[CH], 3)
        } else if starts("sch") {
            (&[S, K], 3)
        } else if starts("igh") {
            (&[AA, IH], 3)
        } else if starts("dge") {
            (&[JH], 3)
        } else if starts("ch") {
            (&[CH], 2)
        } else if starts("sh") {
            (&[SH], 2)
        } else if starts("th") {
            (&[TH], 2)
        } else if starts("ph") {
            (&[F], 2)
        } else if starts("wh") {
            (&[W], 2)
        } else if starts("ck") {
            (&[K], 2)
        } else if starts("ng") {
            (&[NG], 2)
        } else if starts("qu") {
            (&[K, W], 2)
        } else if starts("gh") {
            (if i == 0 { &[G] } else { &[] }, 2)
        } else if i == 0 && (starts("kn") || starts("gn")) {
            (&[N], 2)
        } else if i == 0 && starts("wr") {
            (&[R], 2)
        } else if starts("mb") && ends_after(2) {
            (&[M], 2)
        } else if starts("ee") || starts("ea") {
            (&[IY], 2)
        } else if starts("oo") {
            (&[UW], 2)
        } else if starts("ou") {
            (&[AA, UH], 2)
        } else if starts("ow") {
            (if ends_after(2) { &[AO, UH] } else { &[AA, UH] }, 2)
        } else if starts("oi") || starts("oy") {
            (&[AO, IH], 2)
        } else if starts("ai") || starts("ay") {
            (&[EH, IH], 2)
        } else if starts("au") || starts("aw") {
            (&[AO], 2)
        } else if starts("ey") && ends_after(2) {
            (&[IY], 2)
        } else if starts("ei") || starts("ey") {
            (&[EH, IH], 2)
        } else if starts("ie") {
            (if ends_after(2) { &[AA, IH] } else { &[IY] }, 2)
        } else if starts("ew") || starts("ue") || starts("ui") {
            (&[UW], 2)
        } else if starts("oa") {
            (&[AO, UH], 2)
        } else if is_vowel(c) && next == Some(b'r') && !after_next.is_some_and(is_vowel) {
            match c {
                b'a' => (&[AA, R], 2),
                b'o' => (&[AO, R], 2),
                _ => (&[ER], 2),
            }
        } else if c == b'y' && i > 0 && next == Some(b'r') {
            (&[ER], 2)
        } else if is_vowel(c) {
            let long = magic_e(&letters, i);
            let last = ends_after(1);
            let sounds: &[Segment] = match c {
                // A final e after a consonant is silent ("plate"), except in a two-letter
                // word ("he").
                b'e' if last && n > 2 => &[],
                b'e' if last => &[IY],
                b'a' if long => &[EH, IH],
                b'e' if long => &[IY],
                b'i' if long => &[AA, IH],
                b'o' if long => &[AO, UH],
                b'u' if long => &[UW],
                b'a' if last => &[AH],
                b'i' if last => &[IY],
                b'o' if last => &[AO, UH],
                b'u' if last => &[UW],
                b'a' => &[AE],
                b'e' => &[EH],
                b'i' => &[IH],
                b'o' => &[AA],
                _ => &[AH],
            };
            (sounds, 1)
        } else if c == b'y' {
            let sounds: &[Segment] = if i == 0 || next.is_some_and(is_vowel) {
                &[Y]
            } else if ends_after(1) && n <= 3 {
                &[AA, IH]
            } else if ends_after(1) {
                &[IY]
            } else {
                &[IH]
            };
            (sounds, 1)
        } else if next == Some(c) {
            // A doubled consonant is one sound.
            i += 1;
            continue;
        } else {
            let soft = next.is_some_and(|n| matches!(n, b'e' | b'i' | b'y'));
            let sounds: &[Segment] = match c {
                b'b' => &[B],
                b'c' if soft => &[S],
                b'c' => &[K],
                b'd' => &[D],
                b'f' => &[F],
                b'g' if soft && ends_after(2) => &[JH],
                b'g' => &[G],
                b'h' if next.is_some_and(is_vowel) => &[HH],
                b'h' => &[],
                b'j' => &[JH],
                b'k' => &[K],
                b'l' => &[L],
                b'm' => &[M],
                b'n' => &[N],
                b'p' => &[P],
                b'q' => &[K],
                b'r' => &[R],
                b's' if i > 0 && is_vowel(letters[i - 1]) && next.is_some_and(is_vowel) => &[Z],
                b's' => &[S],
                b't' => &[T],
                b'v' => &[V],
                b'w' => &[W],
                b'x' if i == 0 => &[Z],
                b'x' => &[K, S],
                _ => &[Z],
            };
            (sounds, 1)
        };
        out.extend_from_slice(sounds);
        i += used;
    }
    Sounds::new(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(word: &str) -> String {
        guess(word).map(|s| s.to_string()).unwrap_or_default()
    }

    #[test]
    fn common_patterns() {
        assert_eq!(g("plex"), "P L EH K S");
        assert_eq!(g("mute"), "M UW T");
        assert_eq!(g("winix"), "W IH N IH K S");
        assert_eq!(g("shower"), "SH AA UH ER");
        assert_eq!(g("chip"), "CH IH P");
        assert_eq!(g("plate"), "P L EH IH T");
        assert_eq!(g("city"), "S IH T IY");
        assert_eq!(g("rock"), "R AA K");
    }

    #[test]
    fn no_letters_no_guess() {
        assert!(guess("123").is_none());
        assert!(guess("").is_none());
    }
}
