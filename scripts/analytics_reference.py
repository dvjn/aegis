"""Small exact-arithmetic oracle for analytics correctness fixtures.

This is deliberately independent of SQLite and the production query implementation.
Inputs are compact facts, never payloads. Timestamps are integer UTC milliseconds.
"""

from dataclasses import dataclass
from fractions import Fraction


@dataclass(frozen=True)
class Request:
    id: str
    owner: str
    provider: str
    started: int
    total_bytes: int
    cost: int | None
    conversation: str | None = None


@dataclass(frozen=True)
class Appearance:
    request: str
    kind: str
    call_id: str | None
    content_hash: str
    bytes: int
    tool: str | None = None
    skill: str | None = None
    multiplicity: int = 1
    definitions_in_container: int = 1


@dataclass
class Total:
    calls: int = 0
    definitions: int = 0
    bytes: Fraction = Fraction(0)
    cost: Fraction = Fraction(0)
    unpriced_appearances: int = 0

    def integers(self):
        # Round only after the final report group has been summed.
        return self.calls, self.definitions, int(self.bytes), int(self.cost)


def identity(request, appearance):
    # Tags prevent a missing-ID content hash from colliding with a literal ID.
    token = ("id", appearance.call_id) if appearance.call_id is not None else (
        "hash", appearance.content_hash
    )
    return request.owner, request.provider, request.conversation, token


def report(requests, appearances, owner, start, end):
    """Return tool totals and skill totals for an inclusive request-start window.

    Attribution examines retained facts, but presence and maximum bytes examine
    only the selected window. Conflicting call attribution remains unresolved.
    """
    by_id = {request.id: request for request in requests}
    if len(by_id) != len(requests):
        raise ValueError("duplicate request ID")
    names = {}
    for appearance in appearances:
        request = by_id[appearance.request]
        if appearance.bytes < 0 or appearance.multiplicity < 1:
            raise ValueError("invalid appearance size or multiplicity")
        if appearance.definitions_in_container < 1:
            raise ValueError("empty definition container")
        if appearance.kind == "call":
            names.setdefault(identity(request, appearance), set()).add(
                (appearance.tool, appearance.skill)
            )
    tools, skills, maxima = {}, {}, {}
    for appearance in appearances:
        request = by_id[appearance.request]
        if request.owner != owner or not start <= request.started <= end:
            continue
        key = identity(request, appearance)
        if appearance.kind == "definition":
            label, skill = appearance.tool, None
            size = Fraction(appearance.bytes, appearance.definitions_in_container)
        elif appearance.kind in ("call", "result"):
            candidates = names.get(key, set())
            label, skill = next(iter(candidates)) if len(candidates) == 1 else (None, None)
            size = Fraction(appearance.bytes)
        else:
            raise ValueError("unknown appearance kind")
        destinations = [(tools, label)]
        if skill is not None:
            destinations.append((skills, skill))
        for groups, name in destinations:
            total = groups.setdefault(name, Total())
            if appearance.kind == "definition":
                total.definitions += appearance.multiplicity
                total.bytes += size * appearance.multiplicity
            if request.cost is None:
                total.unpriced_appearances += appearance.multiplicity
            elif request.total_bytes > 0:
                total.cost += Fraction(request.cost, request.total_bytes) * size * appearance.multiplicity
        if appearance.kind != "definition":
            presence = key, appearance.kind
            previous = maxima.get(presence)
            if previous is None or size > previous[0]:
                maxima[presence] = size, label, skill
    for (_, kind), (size, label, skill) in maxima.items():
        tools[label].bytes += size
        tools[label].calls += kind == "call"
        if skill is not None:
            skills[skill].bytes += size
            skills[skill].calls += kind == "call"
    return tools, skills
