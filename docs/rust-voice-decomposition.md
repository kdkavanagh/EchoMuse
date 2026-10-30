# Decomposing the Rust voice controller

Status: implemented after approval. The public API, wire formats, speech policy and threading model remain unchanged. The ownership rules and transition invariants below describe the implementation.

## 1. Decision

Keep `Voice` as the session coordinator and public facade. Give wake handling, conversation handling and output handling private owners. Group the existing session audio mechanisms behind a concrete `SessionAudio` owner inside the coordinator.

These are ordinary Rust values called synchronously on the existing device executor. They are not new tasks, threads, services or channels. Keep `TurnPolicy`, `WakeDecider`, `Uplink`, `RenderClient` and `MediaPlayer`; do not replace them with a generic state-machine framework.

The goal is that one component can change its internal state without editing another component's fields. Moving more `impl Voice` blocks into separate files would not achieve that.

## 2. Where the code lives

- [`Voice`](../controller-rs/crates/voice/src/machine/mod.rs) keeps session facts, capture permission, presentation, diagnostics and the effect sink. It routes each input and admits accepted wakes synchronously.
- [`SessionAudio`](../controller-rs/crates/voice/src/session.rs) keeps the stream registry, subscriptions, worker cleanup scope and pending acknowledgements, including released focus leases. The same module defines the narrow resources and facts each component may borrow.
- [`Wake`](../controller-rs/crates/voice/src/wake.rs) keeps candidates and near-miss recordings. An accepted candidate leaves as a `WakeTransfer` that moves its analysis and scope.
- [`Conversation`](../controller-rs/crates/voice/src/conversation/mod.rs) keeps turns, replies, early-answer watches, lingering records, service calls and repeat context. Its child modules implement those operations without exposing their fields.
- [`supersede`](../controller-rs/crates/voice/src/conversation/turn.rs) returns an owned `ReplyContext` when a wake replaces a reply window. The new turn consumes it; there is no `pending_reply_context` field.
- [`Output`](../controller-rs/crates/voice/src/output.rs) keeps playback and media state. Render events produce typed notices that identify the affected turn; conversation checks that identity before changing a turn.
- [`settle`](../controller-rs/crates/voice/src/machine/mod.rs) still sends uplink commands, delivers subscribed audio and handles render events before updating presentation, with one bound of 64 iterations.

## 3. Ownership

The names below describe private implementation types, not new public API.

| Owner | State it owns | What it must not own |
|---|---|---|
| `Voice` | Session facts and permission; current settings; capability and HA metadata; identity allocation; the three components; presentation, diagnostic mode and the existing effect/evidence sink | Candidate or conversation phase fields, render entries or media internals |
| `SessionAudio` | The existing stream registry and `Uplink`; subscription command acknowledgements; shared operations for opening, converting and closing analysed subscriptions; releases of focus leases awaiting acknowledgement | A second copy of candidate/turn state, endpointing rules or a new inference queue |
| `Wake` | The pending candidate, `WakeDecider`, candidate analysis and verification state; near-miss recording state; refused/outclaimed candidate records | The active turn, reply expectations, playback state or arbitration grants |
| `Conversation` | The active `Turn`, its `TurnPolicy` and service-call map; reply window; early-answer watch; reply context and chain; lingering turns and their records; input focus and the turn's arbitration grant; repeat context | Stream validation, render buffering, preview state or media-player internals |
| `Output` | `RenderClient`, `MediaPlayer`, playout entries, output focus, decode correlation, previews, earcons and announcements | `Turn`, `ReplyWindow`, `QuestionWatch`, their fields or their terminal decisions |

These owners are concurrent roles, not mutually exclusive session stages. A wake can be verified while an older answer plays; conversation can watch for an early answer while output speaks; an ended turn can retain its response recording while a new turn runs. Do not replace those combinations with one flat session-state enum.

### A refinement to the earlier outline: repeat context

`Conversation` keeps `LastAnswer`, not `Output`. It contains a follow-up expectation as well as an audio URL and completion time. Output reports that a spoken answer drained, with its URL and turn identity; conversation decides whether that answer becomes repeatable and keeps its follow-up context. Output never reads conversation expectations.

### Shared mechanisms are not shared policy state

`SessionAudio` is a concrete owner of existing mechanisms, not a general-purpose context object. Its methods cover stream validation, timeline reads, subscriptions, grant installation, analysis open/close and acknowledgement bookkeeping. In particular, opening or converting a subscription installs its backfill grants before later frames can be checked.

Components may borrow these mechanisms and the existing effect sink during a call. They may also receive existing narrow values such as `ArrivalConditions`, `CommandContext`, a settings snapshot or a playback summary. They do not receive `&mut Voice`, another component's mutable fields, or an all-purpose context containing the whole application.

The implementation uses separate `WakeCx`, `ConversationCx` and `OutputCx` borrows. Output receives no capture permission, arbiter, grammar, audio registry or presentation state. Wake receives no arbiter or Home Assistant metadata. Conversation receives the arbiter so it can release the exact grant its turn holds. These borrows contain no component policy state.

Keep `Analysis` and `Evidence` as shared data and algorithms. Their common helpers take the particular analysis supplied by its owner; they do not discover it by reading every component's fields. Wake and conversation own their analysis instances. This avoids copying the existing analysis pipeline into both components or moving all policy state into `SessionAudio`.

Live focus leases stay with the operation that owns them. Once a lease is released, its pending acknowledgement belongs to the session's bookkeeping rather than to an unrelated playback.

The existing shared arbiter remains a synchronous shared service. Admission obtains a grant at the same point as today and moves it into the turn; conversation releases that exact grant when the turn ends. No new arbiter task or delayed release queue is introduced.

## 4. Interfaces and dependency direction

### Public facade

Keep the current `Voice::new`, `handle`, `drain_effects`, `next_deadline`, `status`, `is_lost`, `set_recording` and `clock_origin` interfaces. Keep public `Input` and `Effect` variants and their external meanings. The runtime in `src/voice_session.rs` must not need to know the new component layout.

### Wake

Wake accepts candidate arrival/end, relevant analysis and verifier results, timers and revocation. It appends existing effects for acknowledgement, worker jobs and records. A completed decision yields an owned resolution:

- refusal, with its existing cleanup and record;
- a verified candidate ready for admission;
- withdrawal because capture or the session ended.

The ready candidate moves its subscription identity, analysis and accumulated evidence, scope, wake trace and verification timing. It does not clone the retained audio or serialize an internal handoff. Consuming the candidate removes wake's ownership without cancelling the scope being transferred.

Refusing or closing a candidate must not cancel the session's parent capture permission. Keep the existing distinction between ending that subscription and revoking capture for the whole session.

### Conversation

Conversation accepts a wake transfer, button start, its stream/worker results, HA/alert/correction results, output notices, timers and revocation. The current `Call` map moves here because its variants already identify requests for a turn. Decode calls remain with output. Call ids and turn scopes are still allocated from one session-wide source.

Conversation can issue narrow output commands such as play an answer, interrupt an answer, change dialog focus or replay an answer. It can read a small output summary for command context. This dependency goes in one direction: output never calls conversation or reads its fields.

A wake during a reply window carries its question, expectation and chain into the new turn as an owned value within conversation. Remove the coordinator's `pending_reply_context`; do not replace it with an untyped scratch field or a second global flag.

### Output

Output handles media/preview commands, decoded audio, render acknowledgements and reports, timers and session loss. It queues the same external effects as today.

When playback affects conversation, output returns a typed notice to the coordinator. The notice carries the turn/playback identity and relevant facts, not references to a `Turn`. The required cases are:

- an answer became audible;
- progress supplies the answer's first-frame timing;
- an answer ended, including its reason and any completed audio URL;
- a completed announcement requests a conversation started by HA.

The coordinator delivers a notice to conversation after the output call releases its borrow. Conversation checks the identity before acting. There are no callbacks into a borrowed conversation and no new asynchronous event bus. Use the existing render/media event queues; do not allocate a second queue or a `Vec` for every audio packet merely to cross a module boundary.

A handler reports whether it consumed work separately from whether it produced a conversation notice. An output event that only updates local state must not make the coordinator stop settling prematurely.

### Presentation and diagnostics

Ring, turn-view and satellite-state projection stay with the coordinator for this refactor. They read summaries from conversation and output. The displayed `ActorState` remains a projection, not a replacement for the actual concurrent states.

Components submit typed `Shown` values for cues and turn-view changes. The coordinator's presenter maps them to ring and turn-view operations immediately, so those external effects keep their place among the other effects. Components cannot borrow the mutable projector or supply a callback that changes it.

Keep one session clock origin, evidence sink and counter vocabulary. Components report existing diagnostic events through that sink; they do not acquire independent clocks, logs of retained audio or new policy authority. Preserve consent, effective sample frontiers, redaction and the ordering of evidence relative to external effects.

## 5. Transitions that cross owners

### Accepted wake

1. Wake consumes its pending candidate and returns the transfer.
2. Admission reads the current output context and applies the existing arbitration rules synchronously: interrupting this speaker's answer bypasses arbitration; an ordinary winner gets a grant; a media-context loser can start only the existing restricted turn.
3. A loser that cannot proceed is finalized by wake under the existing outclaimed behavior. A cancelled scope closes without starting a turn.
4. Conversation consumes an admitted transfer. It preserves any reply context, supersedes the previous turn and requests the same output interruption as today.
5. The candidate subscription is converted in place. Its analysis, evidence and scope move into the new turn. Preserve the current generation, backfill, focus and cue behavior.
6. If conversion fails, close its analysis and release any acquired grant. Do not strand a subscription or install a partial active turn.

Do not add a new arbitration rule or make acquisition/release wait for another task.

### Early answer while a question plays

1. Output reports that the question became audible. Conversation verifies the turn id and starts its early-answer watch.
2. Conversation detects an early answer from the watch's evidence while output may still be playing.
3. In one synchronous transition, it records the asking turn's ending, preserves the expectation and chain, requests interruption, and transfers the watch's subscription/analysis/scope into the reply turn.
4. A later playback-end notice for the question cannot end the new turn or open another reply window.

Keep `Turn`, reply windows and early-answer handling in the same component initially. Splitting them into three autonomous machines would make this transfer harder rather than improve ownership.

### Turn completion and lingering records

An ended turn may remain in conversation's lingering collection while the next turn runs. Keep its response subscription, recording and continuation associated with its original identity. Completion of old output or a late response block must not overwrite the new active turn. Keep the existing response close deadlines and record-publication rules.

### Mute and session loss

The coordinator revokes capture before asking components to finish. Preserve the current distinction between mute and session loss: mute revokes input; session loss also closes all output and ends the session. Do not turn mute into a new stop-media policy.

Keep the current ordering that snapshots authorized turn evidence before erasing the response audio, ends lingering records, revokes subscriptions and worker input, and finishes candidate/turn/reply state. A late result cannot dispatch a new HA action or revive revoked capture. Unmute receives a fresh scope; it does not reactivate an old one.

## 6. Ordering and performance invariants

- There is one event sequencer, the existing device executor, and one `Voice::handle` entry point. Components never await I/O.
- Preserve observed ingress order, sample/epoch distinctions, worker-result order and external effect ordering. Do not insert an asynchronous handoff between verification and admission or between an early answer and its reply.
- Route stream results by their current owner and the existing `StreamKey`. A valid mic/reference result queued before candidate-to-turn transfer must still reach the transferred analysis. A verifier result for an already-decided candidate remains stale. Do not freeze the original owner into a new job tag and discard legitimate transferred results.
- Keep the current settle sequence: uplink commands, subscribed deliveries, render events, then presentation. Process component notices immediately at the corresponding point in that sequence. Preserve the existing settle bound; internal handoffs must not consume extra iterations or multiply it per component.
- `next_deadline` remains the minimum of the existing component/resource deadlines. Do not add a polling tick or change timeout values.
- Preserve the distinction between capture scopes, turn scopes and the session scope used for worker cleanup. Closing an analysis must still work after its input scope was cancelled.
- Move owned state and existing `Arc` buffers. Add no PCM copy, dynamic trait layer, allocation per packet, or per-component command channel.
- Normal utterances through 15 seconds and the existing extended option keep their current span budget and audio boundaries. This refactor does not retune wake, echo, endpoint, correction or routing policy.
- Keep wire formats, capability negotiation, durable outcomes, HA no-resend rules, database formats and the four-worker scheduling model unchanged.

## 7. Module boundaries and migration

The component modules are private. Conversation's turn/reply/early/local/record/linger helpers are children of its module so they can collaborate without exposing their fields to the rest of the crate. Output imports no conversation state types; wake imports no conversation owner. Small owned values cross those boundaries. No getter exposes mutable component policy state.

The cutover removed the old sibling `impl Voice` modules rather than leaving a second implementation. `machine/` contains the coordinator, button handling and diagnostic subscriptions; `conversation/` contains conversation policy. Shared analysis algorithms remain in `analysis.rs`, native AFE data and chart algorithms in `afe.rs`, and row-format helpers in `rows.rs`.

The migration followed the approved order: shared audio mechanisms and output contracts, wake transfer, output notices, then conversation state and removal of the old paths. The public facade stayed unchanged throughout. The final implementation has no compatibility aliases, temporary bridges, test-only session API or duplicate component state.

## 8. Verification and acceptance

Use the current public `Voice` simulation and controller/device-link tests. Do not add tests of component field layout, module names, helper calls, exact private counters or copied defaults. Extend existing scenarios only where a boundary case lacks consumer coverage.

| Scenario | Required observable result |
|---|---|
| Accepted wake with analysis already queued | The full utterance reaches the new turn; valid transferred analysis is not lost or processed twice. |
| Rejected or outclaimed wake | The same refusal/restriction, device messages and retained record occur; no ordinary HA intent is sent by an ineligible speaker. |
| A wake during a reply window | The question and expectation carry into the new turn exactly once. |
| An early answer during output | The question stops, the reply uses the existing analysis, and a late end for the question does not stop the reply. |
| Streaming HA answer and playback overlap | Playback may begin before the HA run ends; neither state falsely terminates the other. |
| Mute/reconnect with work pending | No revoked result acts on the current session, authorized final metadata survives, and unmute/reconnect uses fresh permission. |
| New turn while an old response lingers | Each recording and continuation remains attached to the correct turn. |
| Media, preview and announcement | Existing interruption/resumption, preview replies and HA-requested conversation behavior remain compatible. |
| Long normal and extended utterances | The retained span and STT copy remain complete and are not rejected by the old live-audio budget. |

Run the workspace tests and clippy with all targets/features, and the formatting check. Exercise the changed paths with a throwaway controller/device simulation rather than relying only on compilation. Compare consumer-visible decisions, audio boundaries, logical identities and external effects; do not demand equality of incidental internal traces or randomly generated ids.

Implementation is accepted when the new owners enforce their boundaries, the public facade and external behavior remain compatible, and the above scenarios pass. Code review must also confirm that no all-purpose mutable context, duplicate owner registry, new asynchronous bus or redundant state was introduced.

## 9. Implementation and verification

The approved design is implemented. Turns, replies, early answers and lingering records remain together in conversation, while output reports playback facts through typed notices. Repeat context remains in conversation because it contains an expectation.

Regression scenarios use the public `Voice` facade, device messages, effects and persisted records rather than reading component fields. They cover queued candidate analysis, a wake during a reply window, a late end for an interrupted question, overlapping lingering records and commands that exceed the former live-audio budget.

A throwaway executable also exercised accepted and outclaimed wakes, queued analysis, reply context, early answers, streaming HA/playback overlap, mute followed by fresh capture permission and a new capture epoch, overlapping recordings, long normal and extended utterances, media, previews and an HA announcement that starts a conversation. The executable was not retained as another session API.

Final checks passed:

- `cargo test --workspace --all-targets --all-features`: 1,154 tests passed; 19 remained ignored.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- `cargo fmt --all -- --check`.

The throwaway simulation passed all eleven scenarios. These checks used simulated devices and tone models; no hardware deployment or real-device acoustic validation was performed.
