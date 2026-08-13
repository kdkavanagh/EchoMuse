# EchoMuse Documentation

User-facing documentation, written to be readable without an engineering
background. Intended as the seed of a future wiki — screenshots and
walkthroughs welcome.

| Document | What it covers |
|---|---|
| [Quickstart](quickstart.md) | Zero to talking to your Dot: controller install, first-run setup, device approval, Home Assistant hookup, everyday use. |
| [Configuration Guide](configuration.md) | Every dashboard setting explained in plain language — what it does, when to touch it, and how to tune it. Ends with [what leaves your network](configuration.md#what-leaves-your-network) — there is no telemetry, and the one outbound connection is named. |
| [The Voice Pipeline, Explained](voice-pipeline.md) | How your voice travels from the microphones to Home Assistant and back, stage by stage, with the benefits and caveats of each design choice. |

Deeper technical references live elsewhere:

- [support-bundle.md](support-bundle.md) — what a support bundle contains,
  what it deliberately excludes, and how to check before you share one.
- [alexa-afe.md](alexa-afe.md) — how the stock Alexa stack does echo
  cancellation and beamforming on the same hardware, where its tuning lives on
  the device, and what of it EchoMuse can and cannot reuse. Nothing from Amazon
  is vendored here; extract from your own Dot.
- [native-afe-migration.md](native-afe-migration.md) — spec and phased plan for
  moving device audio onto that native front end. Proposal, not started.
- [rooting.md](rooting.md) — what a device needs before EchoMuse can use it.
  The exploit itself is R0rt1z2's work on XDA Forums and that thread is canon;
  this covers where EchoMuse picks up, and what the wizard does for you.
- [SETUP.md](../SETUP.md) — architecture reference: how the mic array, the
  audio pipeline and the device/controller protocol actually work, plus
  troubleshooting. Not an onboarding guide.
- [JOURNAL.md](../JOURNAL.md) — the engineering journal: a long-form,
  chronological record of what was built, what broke, and what we got wrong.
- [CLAUDE.md](../CLAUDE.md) — codebase orientation for developers (and AI
  assistants).
