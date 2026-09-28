# EchoMuse Documentation

User-facing documentation, written to be readable without an engineering
background. Intended as the seed of a future wiki — screenshots and
walkthroughs welcome.

| Document | What it covers |
|---|---|
| [Quickstart](quickstart.md) | Zero to talking to your Dot: controller install, Home Assistant token, first-run setup, device approval, Home Assistant hookup, everyday use. |
| [Configuration Guide](configuration.md) | Every dashboard setting explained in plain language — what it does, when to touch it, and how to tune it. Ends with [what leaves your network](configuration.md#what-leaves-your-network) — there is no telemetry, and every outbound connection is named. |
| [The Voice Pipeline, Explained](voice-pipeline.md) | How your voice travels from the microphones to Home Assistant and back, stage by stage, with the benefits and caveats of each design choice. |
| [LED Ring States](led-ring-states.md) | What every colour and animation on the ring means, and which side (Dot or controller) draws it. |

Deeper technical references live elsewhere:

- [post-afe-audio-architecture.md](post-afe-audio-architecture.md) — the
  architecture EchoMuse implements above the native AFE: on-device BCResNet
  wake detection, playback-aware barge-in, endpointing, continuation,
  HA-native satellite timers, and stock Local Calendar-backed durable alarms.
  Sections 16 (normative contracts) and 18 (what replaced what) are the
  most specific.
- [protocol-v1.md](protocol-v1.md) — the device ↔ controller wire protocol:
  the three WebSockets, every JSON message, the EMA1 audio frame, and the
  capability set.
- [support-bundle.md](support-bundle.md) — what a support bundle contains,
  what it deliberately excludes, and how to check before you share one.
- [playback-capture.md](playback-capture.md) — script-driven capture: play a
  corpus through a Dot and get back exactly what its microphones heard, for
  building device-specific training and test sets.
- [rooting.md](rooting.md) — what a device needs before EchoMuse can use it.
  The exploit itself is R0rt1z2's work on XDA Forums and that thread is canon;
  this covers where EchoMuse picks up, and what the wizard does for you.
- [native-afe-migration.md](native-afe-migration.md) — how device audio came
  to run on the Echo's native front end, and what the HAL owns that used to be
  configurable. History.
- [alexa-afe.md](alexa-afe.md) — how the stock Alexa stack does echo
  cancellation and beamforming on the same hardware, where its tuning lives on
  the device, and what of it EchoMuse can and cannot reuse. Nothing from Amazon
  is vendored here; extract from your own Dot.
- [alexa-endpointing.md](alexa-endpointing.md) — how the stock stack decides
  that a request has ended.
- [alexa-turn-control.md](alexa-turn-control.md) — how the stock stack handles
  being interrupted mid-response, recognising "stop" during a turn, and
  deciding whether follow-up speech was addressed to the device, with the
  recovered threshold values. Research input to the architecture above.
- [alexa-alerts-and-leds.md](alexa-alerts-and-leds.md) — the HeadlessBeacon
  alarm/timer runtime (Doze-exempt scheduling, persistence, reboot recovery,
  focus, stop/snooze and loops), the alert sounds, and the 284-file LED
  animation corpus including the plain-text 12-LED DSL compared with
  EchoMuse's parametric `led_anim`.
- [CLAUDE.md](../CLAUDE.md) — codebase orientation for developers (and AI
  assistants).
