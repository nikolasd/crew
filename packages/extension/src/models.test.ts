// Tests for model-name resolution.
//
// The two fixtures below are the COMPLETE provider lists from
// `omp models ls --json` (2026-09-07: anthropic 24, openai-codex 6), not a
// selection from them. That matters more than it looks: a first draft of
// this file used a hand-picked subset, and in the subset `opus` matched one
// id and so resolved by unique substring match -- making the alias table
// look optional. Against the real 24 it matches 10, `sonnet` 8, `fable` 2
// and `haiku` 3, so every one of claude's four aliases is load-bearing. A
// subset fixture can make an ambiguous input look unique, which is the one
// thing these tests exist to catch.

import { expect, test } from "bun:test";
import { type Catalogue, decideModel, isCataloguedAdapter, readCatalogue, resolutionNote, resolveModelName } from "./models";

/** Every `anthropic` id omp catalogues. */
const ANTHROPIC: Catalogue = {
  available: true,
  ids: [
    "claude-3-5-sonnet-20240620",
    "claude-3-5-sonnet-20241022",
    "claude-3-haiku-20240307",
    "claude-fable-5",
    "claude-fable-5-1",
    "claude-haiku-4-5",
    "claude-haiku-4-5-20251001",
    "claude-mythos-5",
    "claude-opus-4-0",
    "claude-opus-4-1",
    "claude-opus-4-1-20250805",
    "claude-opus-4-20250514",
    "claude-opus-4-5",
    "claude-opus-4-5-20251101",
    "claude-opus-4-6",
    "claude-opus-4-7",
    "claude-opus-4-8",
    "claude-opus-5",
    "claude-sonnet-4-0",
    "claude-sonnet-4-20250514",
    "claude-sonnet-4-5",
    "claude-sonnet-4-5-20250929",
    "claude-sonnet-4-6",
    "claude-sonnet-5",
  ],
};

/** Every `openai-codex` id omp catalogues. */
const CODEX: Catalogue = { available: true, ids: ["gpt-5.4-mini", "gpt-5.5", "gpt-5.6-luna", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-6-astra"] };

test("an exact canonical id resolves as exact, with no note", () => {
  const r = resolveModelName("codex", "gpt-5.6-sol", CODEX);
  expect(r).toEqual({ kind: "exact", model: "gpt-5.6-sol" });
  expect(resolutionNote("codex", r)).toBeUndefined();
});

test("the maintainer's example -- `sol` resolves with no alias entry for it", () => {
  const r = resolveModelName("codex", "sol", CODEX);
  expect(r).toEqual({ kind: "match", model: "gpt-5.6-sol", from: "sol" });
  expect(resolutionNote("codex", r)).toContain("gpt-5.6-sol");
  expect(resolutionNote("codex", r)).toContain("only openai-codex model");
});

// This is the test that pins the resolution ORDER, and it is the reason the
// order is what it is. `haiku` matches three real anthropic ids, so a
// substring-first resolver would call the vendor's own unambiguous shorthand
// ambiguous and refuse it. Swap the alias step after the substring step and
// this test fails.
test("a vendor alias wins over an ambiguous substring match", () => {
  // Each of claude's four aliases matches several catalogue ids as a
  // substring, so a substring-first resolver would refuse all four as
  // ambiguous. The vendor's own answer has to win.
  const ambiguity = { fable: 2, haiku: 3, sonnet: 8, opus: 10 };
  for (const [alias, count] of Object.entries(ambiguity)) {
    expect(ANTHROPIC.available && ANTHROPIC.ids.filter((id) => id.includes(alias)).length).toBe(count);
  }

  expect(resolveModelName("claude", "haiku", ANTHROPIC)).toEqual({ kind: "alias", model: "claude-haiku-4-5", from: "haiku" });
  expect(resolveModelName("claude", "opus", ANTHROPIC)).toEqual({ kind: "alias", model: "claude-opus-5", from: "opus" });
  expect(resolveModelName("claude", "sonnet", ANTHROPIC)).toEqual({ kind: "alias", model: "claude-sonnet-5", from: "sonnet" });
  expect(resolveModelName("claude", "fable", ANTHROPIC)).toEqual({ kind: "alias", model: "claude-fable-5-1", from: "fable" });
});

test("a model with no alias resolves by unique substring match in the 2026-09-07 snapshot", () => {
  // `mythos` is the useful case: no alias entry, and exactly one anthropic
  // id contains it. Same mechanism that resolves codex's `sol`.
  //
  // "in the snapshot", not "in the real catalogue": this asserts against
  // the frozen fixture above, deliberately, so the suite does not depend on
  // the machine's omp install. When anthropic ships a second `mythos` the
  // real catalogue makes this input ambiguous and this test still passes --
  // correct for a unit test, so the name has to say which of the two it is
  // measuring rather than claim the one it is not.
  expect(resolveModelName("claude", "mythos", ANTHROPIC)).toEqual({ kind: "match", model: "claude-mythos-5", from: "mythos" });
});

test("`haiku` is in the alias table at all -- `claude --help` lists only three of the four", () => {
  for (const alias of ["fable", "opus", "sonnet", "haiku"]) {
    const r = resolveModelName("claude", alias, ANTHROPIC);
    expect(r.kind).toBe("alias");
  }
});

test("an ambiguous substring refuses and names every candidate, rather than guessing", () => {
  const r = resolveModelName("codex", "gpt-5.6-", CODEX);
  expect(r.kind).toBe("ambiguous");
  if (r.kind === "ambiguous") {
    expect(r.candidates).toEqual(["gpt-5.6-luna", "gpt-5.6-sol", "gpt-5.6-terra"]);
  }
});

test("an unknown name passes through UNVERIFIED and says so -- never silently accepted", () => {
  const r = resolveModelName("codex", "gpt-5.9-hallucinated-20260101", CODEX);
  expect(r).toEqual({ kind: "unverified", model: "gpt-5.9-hallucinated-20260101", why: "not-in-catalogue" });
  const note = resolutionNote("codex", r);
  expect(note).toContain("UNVERIFIED");
  expect(note).toContain("not in omp's catalogue");
});

test("an unavailable catalogue passes through unverified -- never refuses everything", () => {
  const r = resolveModelName("codex", "gpt-5.6-sol", { available: false, why: "omp not on PATH" });
  expect(r.kind).toBe("unverified");
  if (r.kind === "unverified") {
    expect(r.why).toBe("catalogue-unavailable");
    expect(r.detail).toBe("omp not on PATH");
  }
  expect(resolutionNote("codex", r)).toContain("could not be verified");
});

test("a vendor alias still resolves when the catalogue is unavailable", () => {
  // The alias table is local, so a broken catalogue must not cost the user
  // shorthands that never needed it.
  const r = resolveModelName("claude", "opus", { available: false, why: "omp not on PATH" });
  expect(r).toEqual({ kind: "alias", model: "claude-opus-5", from: "opus" });
});

// The three catalogue-unavailable shapes. The third is the one worth having
// a test for: an empty provider list reads as a clean answer, and "no models
// exist for codex" is never true, so it has to be treated as the instrument
// failing rather than as a validation result.
test("a catalogue read failure is unavailable, not empty", async () => {
  const c = await readCatalogue("codex", async () => {
    throw new Error("spawn omp ENOENT");
  });
  expect(c.available).toBe(false);
  if (!c.available) expect(c.why).toContain("could not run");
});

test("non-JSON output is unavailable, not empty", async () => {
  const c = await readCatalogue("codex", async () => "not json at all");
  expect(c.available).toBe(false);
  if (!c.available) expect(c.why).toContain("did not return JSON");
});

test("JSON without a `models` array is unavailable -- a shape change is not an answer", async () => {
  const c = await readCatalogue("codex", async () => JSON.stringify({ somethingElse: [] }));
  expect(c.available).toBe(false);
  if (!c.available) expect(c.why).toContain("shape has changed");
});

test("an EMPTY provider list is the instrument failing, not a clean answer", async () => {
  // A catalogue that parses, has the right shape, and lists models for
  // other providers but none for ours. Reading that as "validated: nothing
  // is valid" would refuse every model name on a broken read.
  const c = await readCatalogue("codex", async () => JSON.stringify({ models: [{ provider: "anthropic", id: "claude-opus-5" }] }));
  expect(c.available).toBe(false);
  if (!c.available) expect(c.why).toContain("no models for provider");
});

test("the catalogue filters to the adapter's own provider", async () => {
  const raw = JSON.stringify({
    models: [
      { provider: "openai-codex", id: "gpt-5.6-sol" },
      { provider: "github-copilot", id: "gpt-5.6-sol" },
      { provider: "openrouter", id: "openai/gpt-5.6-sol-pro" },
    ],
  });
  const c = await readCatalogue("codex", async () => raw);
  expect(c).toEqual({ available: true, ids: ["gpt-5.6-sol"] });
});

test("ompRpc has no single provider, so it is unavailable by construction rather than mis-scoped", async () => {
  const c = await readCatalogue("ompRpc", async () => {
    throw new Error("should not be called");
  });
  expect(c.available).toBe(false);
  if (!c.available) expect(c.why).toContain("no single catalogue provider");
});

// ------------------------------------------------- decideModel: resolution
// meets the stored-model rules
//
// `crew_profile` already refuses an explicit model that disagrees with the
// one recorded in `.omp/crew.json`. Resolution has to happen BEFORE that
// comparison, and on BOTH sides of it: a stored `opus` and an explicit
// `claude-opus-5` name one model, and reading them as a conflict would
// refuse a correct call. The comparison is between canonical ids, never
// between spellings.

const STORED_NONE = undefined;

test("an alias resolves to the canonical id the vendor gets, not the spelling the caller typed", () => {
  const d = decideModel("claude", "sonnet", STORED_NONE, ANTHROPIC);
  expect(d).toEqual({ kind: "use", model: "claude-sonnet-5", verified: true, note: expect.stringContaining("alias") });
});

test("a stored shorthand and an explicit canonical id are the same model, not a conflict", () => {
  // The direction the spec did not mention. `.omp/crew.json` written before
  // Existing files hold shorthands, so this is the common case.
  const d = decideModel("claude", "claude-opus-5", "opus", ANTHROPIC);
  expect(d.kind).toBe("use");
  if (d.kind === "use") expect(d.model).toBe("claude-opus-5");
});

test("a stored canonical id and an explicit shorthand are the same model, not a conflict", () => {
  const d = decideModel("claude", "opus", "claude-opus-5", ANTHROPIC);
  expect(d.kind).toBe("use");
  if (d.kind === "use") expect(d.model).toBe("claude-opus-5");
});

test("a genuine disagreement is still a conflict, reporting the RAW stored text", () => {
  // Raw, not canonical: the correction path is "edit .omp/crew.json", so the
  // error has to name what the reader will actually find in the file.
  const d = decideModel("claude", "sonnet", "opus", ANTHROPIC);
  expect(d).toEqual({ kind: "conflict", configuredModel: "opus" });
});

test("a dated snapshot and a floating alias are different models, so they conflict", () => {
  const d = decideModel("claude", "haiku", "claude-haiku-4-5-20251001", ANTHROPIC);
  expect(d).toEqual({ kind: "conflict", configuredModel: "claude-haiku-4-5-20251001" });
});

test("an ambiguous request is reported as ambiguous, never as a conflict with the stored model", () => {
  // Ambiguity has to be decided before the comparison. Otherwise the
  // unresolvable input falls through to a string compare against the stored
  // value, and the user is told the wrong thing about a wrong thing.
  const d = decideModel("codex", "gpt-5.6-", "gpt-5.6-sol", CODEX);
  expect(d.kind).toBe("ambiguous");
  if (d.kind === "ambiguous") expect(d.candidates).toEqual(["gpt-5.6-luna", "gpt-5.6-sol", "gpt-5.6-terra"]);
});

test("an unrecognised model is used, with the UNVERIFIED note attached", () => {
  const d = decideModel("codex", "gpt-5.9-invented", STORED_NONE, CODEX);
  expect(d.kind).toBe("use");
  if (d.kind === "use") {
    expect(d.model).toBe("gpt-5.9-invented");
    expect(d.note).toContain("UNVERIFIED");
  }
});

test("an exact canonical id carries no note -- nothing was resolved", () => {
  expect(decideModel("codex", "gpt-5.6-sol", STORED_NONE, CODEX)).toEqual({ kind: "use", model: "gpt-5.6-sol", verified: true });
});

test("only the adapters omp catalogues are resolvable -- a custom adapter is left alone", () => {
  expect(isCataloguedAdapter("claude")).toBe(true);
  expect(isCataloguedAdapter("codex")).toBe(true);
  expect(isCataloguedAdapter("copilot")).toBe(true);
  // ompRpc has no provider and no aliases, so resolving it could only ever
  // annotate a name it cannot check. Excluded here so `crew_profile`'s
  // behaviour for it is byte-identical to before resolution existed.
  expect(isCataloguedAdapter("ompRpc")).toBe(false);
  expect(isCataloguedAdapter("terminalDegraded")).toBe(false);
});

// `verified` exists to gate PERSISTENCE, not registration. The
// symptom was an invented dated id becoming the repository's durable answer
// in `.omp/crew.json`; a name omp's catalogue does not know is exactly that
// value, so it may run but must not be written down.
test("a resolved model is verified -- it may be persisted", () => {
  for (const [input, model] of [
    ["gpt-5.6-sol", "gpt-5.6-sol"],
    ["sol", "gpt-5.6-sol"],
    ["gpt-5.6", "gpt-5.6-sol"],
  ]) {
    const d = decideModel("codex", input!, undefined, CODEX);
    expect(d).toEqual({ kind: "use", model, verified: true, ...(input === "gpt-5.6-sol" ? {} : { note: expect.any(String) }) });
  }
});

test("an unverified model is NOT verified -- it runs but is not written to crew.json", () => {
  const d = decideModel("codex", "gpt-5.9-invented", undefined, CODEX);
  expect(d.kind === "use" && d.verified).toBe(false);
});

test("a model resolved from the local alias table is verified even when the catalogue is unavailable", () => {
  // The alias table is crew's own, checked in and reviewed, so a broken
  // catalogue does not make its targets unknown.
  const d = decideModel("claude", "opus", undefined, { available: false, why: "omp not on PATH" });
  expect(d).toEqual({ kind: "use", model: "claude-opus-5", verified: true, note: expect.stringContaining("alias") });
});

test("a name that could not be checked because the catalogue was unavailable is not verified", () => {
  const d = decideModel("codex", "gpt-5.6-sol", undefined, { available: false, why: "omp not on PATH" });
  expect(d.kind === "use" && d.verified).toBe(false);
});
