"""Wake candidate open rules and shadow-rule reports (SPEC §5.2, WIRE §4.1/§4.4).

A device announcing `open_rules_v1` opens a wake candidate when any of its
live open rules fires and evaluates shadow rules it never acts on. The
registry baseline — the 3-window mean at each profile's threshold — is always
the first live rule, so extra rules can only add opens. This module owns the
typed rule, the two config keys that hold the extra and shadow rules, the
`wake.stats.shadow` report the device sends, and the Poisson bound the
dashboard shows next to a shadow rule's would-be false wakes.
"""
from __future__ import annotations

import enum
import math
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import TypedDict

import em_audio_timeline as tl

OPEN_RULES_KEY = "wakeOpenRules"
SHADOW_RULES_KEY = "wakeShadowRules"

MAX_EXTRA_OPEN_RULES = 4
MAX_SHADOW_RULES = 8
MIN_THRESHOLD = 0.30
MAX_THRESHOLD = 0.99
MAX_WINDOWS = 3
BASELINE_WINDOWS = 3

HOP_MS = 160                    # one scored hop: two 80 ms capture blocks
LEAD_BUCKETS = 7                # lead histogram: −3..+3 hops at index lead+3
LEAD_OFFSET = (LEAD_BUCKETS - 1) // 2


class RuleProfile(enum.StrEnum):
    """The threshold profile a rule applies at (SPEC §5.3)."""

    IDLE = "idle"
    PLAYBACK = "playback"


class RuleCombine(enum.StrEnum):
    """How a rule combines its windows: their mean, or every one, against the threshold."""

    MEAN = "mean"
    ALL = "all"


class RuleError(ValueError):
    """An open/shadow rule or rule list that is not acceptable."""


def _int(value: object) -> int | None:
    return value if isinstance(value, int) and not isinstance(value, bool) else None


def _float(value: object) -> float | None:
    if isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value):
        return float(value)
    return None


@dataclass(frozen=True, slots=True)
class OpenRule:
    """One candidate-open condition: at a scored hop of `profile`, over the last
    `windows` raw scores, their mean (or every one) reaches `threshold`."""

    profile: RuleProfile
    windows: int
    combine: RuleCombine
    threshold: float

    @property
    def key(self) -> str:
        """Canonical persistence/UI key, e.g. `idle:2:mean:0.95`."""
        return f"{self.profile}:{self.windows}:{self.combine}:{self.threshold:.2f}"

    def wire(self) -> dict[str, object]:
        return {"profile": str(self.profile), "windows": self.windows,
                "combine": str(self.combine), "threshold": self.threshold}

    @classmethod
    def parse(cls, raw: object) -> OpenRule:
        """A rule as the device reports it (any threshold in (0, 1]). Raises RuleError."""
        if not isinstance(raw, Mapping):
            raise RuleError("a rule must be an object")
        profile, combine = raw.get("profile"), raw.get("combine")
        windows, threshold = _int(raw.get("windows")), _float(raw.get("threshold"))
        if not isinstance(profile, str) or profile not in RuleProfile:
            raise RuleError(f"rule profile must be idle or playback, got {profile!r}")
        if not isinstance(combine, str) or combine not in RuleCombine:
            raise RuleError(f"rule combine must be mean or all, got {combine!r}")
        if windows is None or not 1 <= windows <= MAX_WINDOWS:
            raise RuleError(f"rule windows must be 1 to {MAX_WINDOWS}, got {raw.get('windows')!r}")
        if threshold is None or not 0.0 < threshold <= 1.0:
            raise RuleError(f"rule threshold must be in (0, 1], got {raw.get('threshold')!r}")
        return cls(RuleProfile(profile), windows, RuleCombine(combine), threshold)

    @classmethod
    def from_config(cls, raw: object) -> OpenRule:
        """A configured rule: threshold 0.30–0.99 with at most two decimals,
        normalized to exactly two. Raises RuleError."""
        rule = cls.parse(raw)
        if not MIN_THRESHOLD <= rule.threshold <= MAX_THRESHOLD:
            raise RuleError(f"rule threshold must be {MIN_THRESHOLD:.2f} to {MAX_THRESHOLD:.2f}, "
                            f"got {rule.threshold}")
        rounded = round(rule.threshold, 2)
        if abs(rounded - rule.threshold) > 1e-9:
            raise RuleError(f"rule threshold must have at most two decimals, got {rule.threshold}")
        return cls(rule.profile, rule.windows, rule.combine, rounded)

    @classmethod
    def from_key(cls, key: str) -> OpenRule | None:
        """The rule a canonical key names, or None when it names none."""
        parts = key.split(":")
        if len(parts) != 4:
            return None
        try:
            return cls.parse({"profile": parts[0], "windows": int(parts[1]),
                              "combine": parts[2], "threshold": float(parts[3])})
        except (RuleError, ValueError):
            return None


def baseline(idle: float, playback: float) -> tuple[OpenRule, OpenRule]:
    """The registry rules: the 3-window mean at each profile's model threshold."""
    return (OpenRule(RuleProfile.IDLE, BASELINE_WINDOWS, RuleCombine.MEAN, idle),
            OpenRule(RuleProfile.PLAYBACK, BASELINE_WINDOWS, RuleCombine.MEAN, playback))


def parse_rule_list(value: object, *, name: str, limit: int) -> tuple[OpenRule, ...]:
    """A configured rule list: at most `limit` valid rules, no two alike. Raises RuleError."""
    if not isinstance(value, list):
        raise RuleError(f"{name} must be a list of rules")
    if len(value) > limit:
        raise RuleError(f"{name} holds at most {limit} rules")
    rules: list[OpenRule] = []
    for raw in value:
        try:
            rule = OpenRule.from_config(raw)
        except RuleError as err:
            raise RuleError(f"{name}: {err}") from None
        if rule in rules:
            raise RuleError(f"{name} lists {rule.key} twice")
        rules.append(rule)
    return tuple(rules)


def validate_config(values: Mapping[str, object],
                    model_baseline: tuple[OpenRule, OpenRule] | None) -> str | None:
    """The error message for invalid rule config keys in `values`, else None.
    `model_baseline` is the baseline of the model the write selects, when known:
    an extra rule equal to it would add nothing."""
    try:
        if OPEN_RULES_KEY in values:
            extra = parse_rule_list(values[OPEN_RULES_KEY], name=OPEN_RULES_KEY, limit=MAX_EXTRA_OPEN_RULES)
            for rule in extra:
                if model_baseline is not None and rule in model_baseline:
                    return f"{OPEN_RULES_KEY}: {rule.key} is the model's baseline rule, always on"
        if SHADOW_RULES_KEY in values:
            parse_rule_list(values[SHADOW_RULES_KEY], name=SHADOW_RULES_KEY, limit=MAX_SHADOW_RULES)
    except RuleError as err:
        return str(err)
    return None


@dataclass(frozen=True, slots=True)
class RuleSet:
    """The configured extra live rules and shadow rules of one effective config."""

    extra: tuple[OpenRule, ...]
    shadow: tuple[OpenRule, ...]

    @classmethod
    def from_config(cls, cfg: Mapping[str, object]) -> RuleSet:
        """Stored config was validated on write; a list that no longer parses is
        treated as empty rather than sent to a device."""
        def rules(key: str, limit: int) -> tuple[OpenRule, ...]:
            try:
                return parse_rule_list(cfg.get(key, []), name=key, limit=limit)
            except RuleError:
                return ()
        return cls(rules(OPEN_RULES_KEY, MAX_EXTRA_OPEN_RULES), rules(SHADOW_RULES_KEY, MAX_SHADOW_RULES))

    def open_rules(self, idle: float, playback: float) -> tuple[OpenRule, ...]:
        """session.ready `open_rules`: the baseline first, then the extras that differ from it."""
        base = baseline(idle, playback)
        return base + tuple(r for r in self.extra if r not in base)


# --- wake.stats shadow --------------------------------------------------------------


class ShadowEventKind(enum.StrEnum):
    """How a shadow episode no live candidate overlapped resolved."""

    UNMATCHED = "unmatched"     # no live candidate within 10 s: a would-be false wake
    RETRIED = "retried"         # a live candidate followed within 10 s: likely a rescued miss


@dataclass(frozen=True, slots=True)
class ShadowEvent:
    kind: ShadowEventKind
    open_sample: int
    mono_ns: int
    peak_raw: float
    raws: tuple[float, ...]


@dataclass(frozen=True, slots=True)
class ShadowReport:
    """One `wake.stats.shadow` entry: a shadow rule's counters for one stats window."""

    rule: OpenRule
    hops: int
    opens: int
    matched: int
    lead_hist: tuple[int, ...]
    unmatched: int
    retried: int
    live_only: int
    events: tuple[ShadowEvent, ...]
    events_dropped: int

    @classmethod
    def parse(cls, raw: object) -> ShadowReport:
        """Raises (RuleError, tl.ProtocolError) for a malformed entry."""
        if not isinstance(raw, Mapping):
            raise RuleError("a shadow entry must be an object")

        def count(key: str) -> int:
            n = _int(raw.get(key))
            if n is None or n < 0:
                raise RuleError(f"shadow.{key} must be a count, got {raw.get(key)!r}")
            return n

        hist = raw.get("lead_hist")
        if not isinstance(hist, list) or len(hist) != LEAD_BUCKETS:
            raise RuleError(f"shadow.lead_hist must hold {LEAD_BUCKETS} counts")
        lead_hist: list[int] = []
        for v in hist:
            n = _int(v)
            if n is None or n < 0:
                raise RuleError("shadow.lead_hist must hold counts")
            lead_hist.append(n)
        events_raw = raw.get("events")
        if not isinstance(events_raw, list):
            raise RuleError("shadow.events must be a list")
        events: list[ShadowEvent] = []
        for ev in events_raw:
            if not isinstance(ev, Mapping):
                raise RuleError("shadow.events entries must be objects")
            kind, peak, raws = ev.get("kind"), _float(ev.get("peak_raw")), ev.get("raws")
            if not isinstance(kind, str) or kind not in ShadowEventKind:
                raise RuleError(f"shadow event kind {kind!r}")
            if peak is None or not isinstance(raws, list):
                raise RuleError("shadow event needs peak_raw and raws")
            values = tuple(f for f in (_float(r) for r in raws) if f is not None)
            if len(values) != len(raws):
                raise RuleError("shadow event raws must be numbers")
            events.append(ShadowEvent(ShadowEventKind(kind), tl.parse_u64(ev.get("open_sample"), "open_sample"),
                                      tl.parse_u64(ev.get("mono_ns"), "mono_ns"), peak, values))
        return cls(
            rule=OpenRule.parse(raw.get("rule")),
            hops=count("hops"), opens=count("opens"), matched=count("matched"),
            lead_hist=tuple(lead_hist),
            unmatched=count("unmatched"), retried=count("retried"), live_only=count("live_only"),
            events=tuple(events), events_dropped=count("events_dropped"),
        )


def parse_shadow(body: Mapping[str, object]) -> tuple[ShadowReport, ...] | None:
    """`wake.stats.shadow`, or None when absent (older firmware: no shadow data,
    never zeros). Malformed entries are dropped."""
    raw = body.get("shadow")
    if not isinstance(raw, list):
        return None
    out: list[ShadowReport] = []
    for entry in raw:
        try:
            out.append(ShadowReport.parse(entry))
        except (RuleError, tl.ProtocolError):
            continue
    return tuple(out)


# --- would-be false-wake rate bound ---------------------------------------------------


def _poisson_cdf(x: int, mu: float) -> float:
    """P(X ≤ x) for X ~ Poisson(mu), summed in log space so large mu cannot underflow."""
    if mu <= 0.0:
        return 1.0
    logs = [-mu + k * math.log(mu) - math.lgamma(k + 1) for k in range(x + 1)]
    top = max(logs)
    return min(1.0, math.exp(top) * sum(math.exp(v - top) for v in logs))


def poisson_upper(x: int, confidence: float = 0.95) -> float:
    """The one-sided upper confidence bound on a Poisson mean after observing
    `x` events: the mu with P(X ≤ x | mu) = 1 − confidence, by bisection
    (x = 0 at 95 %: −ln 0.05 ≈ 2.996)."""
    if x < 0:
        raise ValueError("x must be a count")
    alpha = 1.0 - confidence
    lo, hi = float(x), float(x) + 10.0
    while _poisson_cdf(x, hi) > alpha:
        lo, hi = hi, hi * 2.0
    for _ in range(200):
        mid = (lo + hi) / 2.0
        if _poisson_cdf(x, mid) > alpha:
            lo = mid
        else:
            hi = mid
        if hi - lo <= 1e-12 * hi:
            break
    return (lo + hi) / 2.0


@dataclass(frozen=True, slots=True)
class ShadowTotals:
    """One shadow rule's persisted counters summed over a time range."""

    rule_key: str
    hops: int
    opens: int
    matched: int
    lead_hist: tuple[int, ...]
    unmatched: int
    retried: int
    live_only: int
    events_dropped: int

    @classmethod
    def empty(cls, rule_key: str) -> ShadowTotals:
        return cls(rule_key, 0, 0, 0, (0,) * LEAD_BUCKETS, 0, 0, 0, 0)


@dataclass(frozen=True, slots=True)
class ShadowEventRecord:
    """One persisted unmatched/retried shadow episode."""

    ts: float
    rule_key: str
    kind: str
    peak_raw: float
    raws: tuple[float, ...]


class ShadowRuleSummary(TypedDict):
    """One row of GET /api/devices/{id}/wake_shadow `rules`."""

    key: str
    rule: dict[str, object] | None
    active: bool
    hours: float
    hops: int
    opens: int
    matched: int
    matched_earlier: int
    mean_lead_ms: float | None
    lead_hist: list[int]
    unmatched: int
    retried: int
    live_only: int
    events_dropped: int
    unmatched_per_hour: float | None
    unmatched_per_hour_upper95: float | None


def summarize(totals: Sequence[ShadowTotals], configured: Sequence[OpenRule]) -> list[ShadowRuleSummary]:
    """Per rule: the configured shadow rules first in their order (zero
    counters when nothing was reported yet), then the inactive rules seen in
    `totals`, by key. Rates are None when no hour was evaluated."""
    by_key = {t.rule_key: t for t in totals}
    active_keys = [r.key for r in configured]
    rules = {r.key: r for r in configured}
    keys = active_keys + sorted(k for k in by_key if k not in rules)
    out: list[ShadowRuleSummary] = []
    for key in keys:
        t = by_key.get(key) or ShadowTotals.empty(key)
        rule = rules.get(key) or OpenRule.from_key(key)
        hours = t.hops * HOP_MS / 1000.0 / 3600.0
        matched = sum(t.lead_hist)
        lead_hops = sum((i - LEAD_OFFSET) * n for i, n in enumerate(t.lead_hist))
        out.append(ShadowRuleSummary(
            key=key, rule=rule.wire() if rule is not None else None, active=key in rules,
            hours=hours, hops=t.hops, opens=t.opens, matched=t.matched,
            matched_earlier=sum(t.lead_hist[LEAD_OFFSET + 1:]),
            mean_lead_ms=lead_hops * HOP_MS / matched if matched else None,
            lead_hist=list(t.lead_hist),
            unmatched=t.unmatched, retried=t.retried, live_only=t.live_only, events_dropped=t.events_dropped,
            unmatched_per_hour=t.unmatched / hours if hours > 0 else None,
            unmatched_per_hour_upper95=poisson_upper(t.unmatched) / hours if hours > 0 else None,
        ))
    return out
