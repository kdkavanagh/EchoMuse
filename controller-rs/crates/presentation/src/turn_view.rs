//! The Echo Show's full-screen turn view (`turn.view`, `docs/protocol-v1.md` §4.9), sent
//! only to a device announcing `turn_view_v1`. The firmware relays each body unchanged to
//! its Kiosk Satellite plugin, which shows it until `done` plus `turnViewLinger` seconds.
//!
//! The whole view goes every time, never a delta: at the turn's `dialog_input` start
//! (`listening`), at dispatch (`thinking` with `heard`), on each streamed answer delta at
//! most [`ANSWER_INTERVAL`] apart with the latest text winning (`answering`), at
//! `intent-end` or EchoMuse's own answer (`answering`, `final`), and at the end of the
//! reply's playback or of a turn with no reply (`done`). A turn that ends any other way
//! than completed sends `done` with no answer. `timer.view` belongs to the alerts service,
//! which owns the timer copies it shows.

use echomuse_protocol::{Capabilities, Capability, ControlBody, MessageType};
use serde::Serialize;
use tokio::time::{Duration, Instant};
use uuid::Uuid;

/// The most bytes of UTF-8 an answer may carry.
pub const MAX_ANSWER_BYTES: usize = 16_384;
/// The shortest time between two streamed answer updates: at most 10 a second.
pub const ANSWER_INTERVAL: Duration = Duration::from_millis(100);
const ELLIPSIS: &str = "…";

/// `turn.view.phase`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ViewPhase {
    /// The microphone is open.
    Listening,
    /// The request was dispatched.
    Thinking,
    /// The answer is arriving or playing.
    Answering,
    /// The turn is over.
    Done,
}

/// The `turn.view` body.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TurnViewBody {
    /// The turn.
    pub turn_id: Uuid,
    /// Where it is.
    pub phase: ViewPhase,
    /// The text sent to Home Assistant, or for a local route the text as heard; `None`
    /// until dispatch.
    pub heard: Option<String>,
    /// The answer so far, at most [`MAX_ANSWER_BYTES`]; `None` until its first text.
    pub answer: Option<String>,
    /// The final speech has replaced streamed text.
    #[serde(rename = "final")]
    pub is_final: bool,
    /// A reply window follows this turn.
    pub follow_up: bool,
}

impl ControlBody for TurnViewBody {
    const MESSAGE_TYPE: MessageType = MessageType::TurnView;
}

/// How a turn ended, for its `done`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ViewEnd {
    /// It completed; `follow_up` when a reply window follows.
    Completed {
        /// A reply window follows.
        follow_up: bool,
    },
    /// It ended any other way (no input, cancelled, an error): the answer is dropped.
    Abandoned,
}

/// One device's turn view. Every method returns the body to send, if any; a device
/// without `turn_view_v1` gets nothing. The runtime calls [`TurnView::on_timer`] at
/// [`TurnView::next_deadline`] to send a held answer update.
#[derive(Clone, Debug)]
pub struct TurnView {
    capable: bool,
    view: Option<TurnViewBody>,
    last_answer_sent: Option<Instant>,
    held: bool,
}

impl TurnView {
    /// The view of a device announcing `capabilities`.
    pub fn new(capabilities: &Capabilities) -> Self {
        Self {
            capable: capabilities.contains(Capability::TurnView),
            view: None,
            last_answer_sent: None,
            held: false,
        }
    }

    /// A turn's `dialog_input` started: a new view, replacing any other.
    pub fn listening(&mut self, turn_id: Uuid) -> Option<TurnViewBody> {
        self.view = Some(TurnViewBody {
            turn_id,
            phase: ViewPhase::Listening,
            heard: None,
            answer: None,
            is_final: false,
            follow_up: false,
        });
        self.last_answer_sent = None;
        self.held = false;
        self.send()
    }

    /// The request was dispatched with `heard`.
    pub fn thinking(&mut self, heard: &str) -> Option<TurnViewBody> {
        let view = self.view.as_mut()?;
        view.phase = ViewPhase::Thinking;
        view.heard = Some(heard.to_owned());
        self.send()
    }

    /// The streamed answer so far. Sent at once unless the previous streamed update went
    /// out less than [`ANSWER_INTERVAL`] ago; then it is held for [`TurnView::on_timer`],
    /// and a later delta replaces it.
    pub fn answer_so_far(&mut self, answer: &str, now: Instant) -> Option<TurnViewBody> {
        let view = self.view.as_mut()?;
        if view.is_final || view.phase == ViewPhase::Done {
            return None;
        }
        view.phase = ViewPhase::Answering;
        view.answer = Some(bounded_answer(answer));
        if self
            .last_answer_sent
            .is_some_and(|at| now < at + ANSWER_INTERVAL)
        {
            self.held = true;
            return None;
        }
        self.held = false;
        self.last_answer_sent = Some(now);
        self.send()
    }

    /// The final answer (`intent-end`'s speech, or EchoMuse's own) replaces streamed text.
    pub fn final_answer(&mut self, answer: &str) -> Option<TurnViewBody> {
        let view = self.view.as_mut()?;
        if view.phase == ViewPhase::Done {
            return None;
        }
        view.phase = ViewPhase::Answering;
        view.answer = Some(bounded_answer(answer));
        view.is_final = true;
        self.held = false;
        self.send()
    }

    /// The turn ended.
    pub fn done(&mut self, end: ViewEnd) -> Option<TurnViewBody> {
        let view = self.view.as_mut()?;
        if view.phase == ViewPhase::Done {
            return None;
        }
        view.phase = ViewPhase::Done;
        match end {
            ViewEnd::Completed { follow_up } => view.follow_up = follow_up,
            ViewEnd::Abandoned => {
                view.answer = None;
                view.is_final = false;
                view.follow_up = false;
            }
        }
        self.held = false;
        self.send()
    }

    /// When a held answer update is due.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.held
            .then(|| self.last_answer_sent.map(|at| at + ANSWER_INTERVAL))
            .flatten()
    }

    /// Sends a held answer update once its interval has passed.
    pub fn on_timer(&mut self, now: Instant) -> Option<TurnViewBody> {
        let due = self.next_deadline().is_some_and(|at| now >= at);
        if !due {
            return None;
        }
        self.held = false;
        self.last_answer_sent = Some(now);
        self.send()
    }

    /// The session ended: the firmware finishes the plugin's view itself.
    pub fn session_lost(&mut self) {
        self.view = None;
        self.held = false;
        self.last_answer_sent = None;
    }

    fn send(&self) -> Option<TurnViewBody> {
        self.capable.then(|| self.view.clone()).flatten()
    }
}

/// `answer` cut to [`MAX_ANSWER_BYTES`]: a longer one is cut at the last word that fits
/// and ends in "…".
fn bounded_answer(answer: &str) -> String {
    if answer.len() <= MAX_ANSWER_BYTES {
        return answer.to_owned();
    }
    let room = MAX_ANSWER_BYTES - ELLIPSIS.len();
    let mut cut = room;
    while !answer.is_char_boundary(cut) {
        cut -= 1;
    }
    let head = &answer[..cut];
    // The cut lands inside a word unless the next character starts a new one.
    let inside_word = answer[cut..]
        .chars()
        .next()
        .is_some_and(|c| !c.is_whitespace());
    let head = match head.rfind(char::is_whitespace) {
        Some(space) if inside_word => &head[..space],
        _ => head,
    };
    format!("{}{ELLIPSIS}", head.trim_end())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn show() -> Capabilities {
        Capabilities::from_names(["turn_view_v1", "timer_view_v1", "kiosk_plugin", "leds"])
    }

    fn id() -> Uuid {
        Uuid::from_u128(0x0b9a3c9e_3a55_4d3e_9d6c_5d2d3c1b2a10)
    }

    #[tokio::test(start_paused = true)]
    async fn a_completed_turn_walks_through_every_phase() {
        let mut view = TurnView::new(&show());
        let listening = view.listening(id()).unwrap();
        assert_eq!(
            serde_json::to_value(&listening).unwrap(),
            json!({"turn_id": "0b9a3c9e-3a55-4d3e-9d6c-5d2d3c1b2a10", "phase": "listening",
                "heard": null, "answer": null, "final": false, "follow_up": false})
        );
        let thinking = view.thinking("is the door locked?").unwrap();
        assert_eq!(thinking.phase, ViewPhase::Thinking);
        assert_eq!(thinking.heard.as_deref(), Some("is the door locked?"));
        let streamed = view.answer_so_far("Yes", Instant::now()).unwrap();
        assert_eq!(streamed.phase, ViewPhase::Answering);
        assert!(!streamed.is_final);
        let last = view.final_answer("Yes, the front door is locked.").unwrap();
        assert!(last.is_final);
        let done = view.done(ViewEnd::Completed { follow_up: true }).unwrap();
        assert_eq!(
            serde_json::to_value(&done).unwrap(),
            json!({"turn_id": "0b9a3c9e-3a55-4d3e-9d6c-5d2d3c1b2a10", "phase": "done",
                "heard": "is the door locked?", "answer": "Yes, the front door is locked.",
                "final": true, "follow_up": true})
        );
        assert_eq!(view.done(ViewEnd::Completed { follow_up: false }), None);
        assert_eq!(
            view.answer_so_far("late", Instant::now()),
            None,
            "nothing after done"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_turn_ending_otherwise_is_done_without_an_answer() {
        let mut view = TurnView::new(&show());
        view.listening(id());
        view.thinking("turn on the");
        view.answer_so_far("I could", Instant::now());
        let done = view.done(ViewEnd::Abandoned).unwrap();
        assert_eq!(done.phase, ViewPhase::Done);
        assert_eq!(done.answer, None);
        assert_eq!(done.heard.as_deref(), Some("turn on the"));
        assert!(!done.follow_up);
        // A no-input turn never got past listening.
        view.listening(Uuid::from_u128(2));
        let done = view.done(ViewEnd::Abandoned).unwrap();
        assert_eq!((done.heard, done.answer), (None, None));
    }

    #[tokio::test(start_paused = true)]
    async fn streamed_answers_go_at_most_ten_a_second_and_the_latest_wins() {
        let mut view = TurnView::new(&show());
        view.listening(id());
        view.thinking("tell me a story");
        let t0 = Instant::now();
        assert!(view.answer_so_far("Once", t0).is_some());
        assert_eq!(view.next_deadline(), None);
        tokio::time::advance(Duration::from_millis(40)).await;
        assert_eq!(view.answer_so_far("Once upon", Instant::now()), None);
        tokio::time::advance(Duration::from_millis(40)).await;
        assert_eq!(view.answer_so_far("Once upon a", Instant::now()), None);
        assert_eq!(view.next_deadline(), Some(t0 + ANSWER_INTERVAL));
        tokio::time::advance(Duration::from_millis(19)).await;
        assert_eq!(view.on_timer(Instant::now()), None, "99 ms is too soon");
        tokio::time::advance(Duration::from_millis(1)).await;
        let held = view.on_timer(Instant::now()).unwrap();
        assert_eq!(held.answer.as_deref(), Some("Once upon a"));
        assert_eq!(view.next_deadline(), None);
        // The next delta 100 ms later goes at once.
        tokio::time::advance(ANSWER_INTERVAL).await;
        assert!(
            view.answer_so_far("Once upon a time", Instant::now())
                .is_some()
        );
        // A held update is overtaken by the final answer, which goes at once.
        tokio::time::advance(Duration::from_millis(10)).await;
        assert_eq!(
            view.answer_so_far("Once upon a time,", Instant::now()),
            None
        );
        let last = view.final_answer("Once upon a time, the end.").unwrap();
        assert!(last.is_final);
        assert_eq!(view.next_deadline(), None);
        assert_eq!(view.on_timer(Instant::now() + ANSWER_INTERVAL), None);
        // Streamed text after the final answer is ignored.
        assert_eq!(
            view.answer_so_far("stray", Instant::now() + Duration::from_secs(1)),
            None
        );
    }

    #[test]
    fn a_device_without_turn_view_gets_nothing() {
        let mut view = TurnView::new(&Capabilities::from_names(["leds", "led_anim"]));
        assert_eq!(view.listening(id()), None);
        assert_eq!(view.thinking("hello"), None);
        assert_eq!(view.final_answer("hi"), None);
        assert_eq!(view.done(ViewEnd::Completed { follow_up: false }), None);
    }

    #[test]
    fn nothing_is_sent_for_a_turn_that_never_opened_or_after_loss() {
        let mut view = TurnView::new(&show());
        assert_eq!(view.thinking("hello"), None);
        view.listening(id());
        view.session_lost();
        assert_eq!(view.done(ViewEnd::Abandoned), None);
    }

    #[test]
    fn long_answers_are_cut_at_a_word() {
        let short = "a".repeat(MAX_ANSWER_BYTES);
        assert_eq!(bounded_answer(&short), short);

        let words = "word ".repeat(4_000);
        let cut = bounded_answer(&words);
        assert!(cut.len() <= MAX_ANSWER_BYTES);
        assert!(cut.ends_with("word…"), "{}", &cut[cut.len() - 12..]);

        // A cut inside a word drops that word.
        let mut text = "x".repeat(MAX_ANSWER_BYTES - 6);
        text.push_str(" abcdefgh");
        assert_eq!(
            bounded_answer(&text),
            format!("{}…", "x".repeat(MAX_ANSWER_BYTES - 6))
        );

        // Multi-byte text is cut on a character boundary, and one long word is cut inside it.
        let wide = "é".repeat(MAX_ANSWER_BYTES);
        let cut = bounded_answer(&wide);
        assert!(cut.len() <= MAX_ANSWER_BYTES && cut.ends_with('…'));
        assert!(cut.trim_end_matches('…').chars().all(|c| c == 'é'));
    }
}
