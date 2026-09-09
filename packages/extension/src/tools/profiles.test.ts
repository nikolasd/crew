import { expect, test } from "bun:test";
import type { AgentToolResult, ExtensionAPI, ExtensionContext } from "@oh-my-pi/pi-coding-agent";
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { z as zod } from "zod/v4";

import { CrewClient, JsonRpcRemoteError } from "../client";
import type { Adapter, Catalogue } from "../models";
import { injectTuiMode, registerProfileTool } from "./profiles";

/**
 * A stub of omp's model catalogue. Every test here supplies one: without it
 * `crew_profile` shells out to the real `omp models ls --json`, which makes
 * these unit tests spawn a subprocess, depend on the machine's installed
 * model set, and take about a second each.
 *
 * The anthropic and openai-codex lists are the COMPLETE provider lists from
 * that catalogue (2026-09-07), not a selection: the interesting cases turn
 * on real collisions, and a subset can make an ambiguous name look unique.
 * See the header of models.test.ts, where a subset did exactly that.
 */
const CATALOGUE: Record<string, readonly string[]> = {
  anthropic: [
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
  "openai-codex": ["gpt-5.4-mini", "gpt-5.5", "gpt-5.6-luna", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-6-astra"],
  // Not the real 59 -- no test here turns on github-copilot's contents.
  "github-copilot": ["gpt-5.6-sol", "claude-sonnet-5"],
};

const PROVIDER: Record<string, string> = { claude: "anthropic", codex: "openai-codex", copilot: "github-copilot" };

async function stubCatalogue(adapter: Adapter): Promise<Catalogue> {
  const ids = CATALOGUE[PROVIDER[adapter] ?? ""];
  return ids === undefined ? { available: false, why: `no single catalogue provider bounds the ${adapter} adapter` } : { available: true, ids };
}

const NO_CATALOGUE = async (): Promise<Catalogue> => ({ available: false, why: "omp not on PATH" });

function fakeClient(handler?: (method: string, params: unknown) => unknown) {
  const calls: Array<{ method: string; params: unknown }> = [];
  const client = {
    calls,
    request: async (method: string, params: unknown): Promise<unknown> => {
      calls.push({ method, params });
      return handler ? handler(method, params) : { profileId: "profile-1", sequence: 1 };
    },
  } as unknown as CrewClient;
  return { client, calls };
}

/**
 * A UI stub for the model-ask dialog. Defaults to "no UI attached" (the
 * fail-closed case most existing tests don't care about); pass `select` to
 * simulate an interactive session, and it records every `select()` call so
 * a test can assert on the title/options/preselection the dialog was
 * actually given.
 */
function fakeUi(select?: (title: string, options: readonly string[], dialogOptions?: { initialIndex?: number }) => string | undefined) {
  const calls: Array<{ title: string; options: readonly string[]; initialIndex: number | undefined }> = [];
  return {
    calls,
    select: async (title: string, options: readonly string[], dialogOptions?: { initialIndex?: number }): Promise<string | undefined> => {
      calls.push({ title, options, initialIndex: dialogOptions?.initialIndex });
      return select?.(title, options, dialogOptions);
    },
  };
}

function fakeExtCtx(cwd: string, ui?: ReturnType<typeof fakeUi>): ExtensionContext {
  return (ui === undefined ? { cwd, hasUI: false } : { cwd, hasUI: true, ui }) as unknown as ExtensionContext;
}

/** A `select` that always confirms whatever row the dialog preselected --
 *  simulates a user who accepts the leader's own suggestion verbatim. */
function confirmPreselected(_title: string, options: readonly string[], dialogOptions?: { initialIndex?: number }): string | undefined {
  return dialogOptions?.initialIndex === undefined ? undefined : options[dialogOptions.initialIndex];
}

function setupProfileTool(client: CrewClient, readModelCatalogue: (adapter: Adapter) => Promise<Catalogue> = stubCatalogue) {
  let execute: ((input: unknown, ctx: ExtensionContext) => Promise<AgentToolResult<unknown>>) | undefined;
  const api = {
    zod,
    logger: { debug() {}, info() {}, warn() {}, error() {} },
    registerTool(tool: { execute: (id: string, input: unknown, s: unknown, o: unknown, c: ExtensionContext) => Promise<AgentToolResult<unknown>> }) {
      execute = (input, c) => tool.execute("id", input, undefined, undefined, c);
    },
  } as unknown as ExtensionAPI;
  const ctx = { getClient: async () => client, readModelCatalogue } as never;
  registerProfileTool(api, ctx);
  return { tool: execute! };
}

function tempRepo(): string {
  return mkdtempSync("/tmp/bat-profiles-test-");
}

function writeRepoConfig(repository: string, contents: string): string {
  const dir = join(repository, ".omp");
  mkdirSync(dir, { recursive: true });
  const path = join(dir, "crew.json");
  writeFileSync(path, contents);
  return path;
}

// ------------------------------------------------------- resolution order

// `sonnet` is claude's own alias, so it resolves to the canonical id the
// daemon receives -- but only once a human confirms it in the dialog, since
// no model is configured yet for this adapter. The leader's suggestion
// preselects the row; `confirmPreselected` simulates a user accepting it.
test("crew_profile resolves an explicit alias to the canonical id the daemon receives, once the dialog confirms it", async () => {
  const repository = tempRepo();
  const { client, calls } = fakeClient();
  const ui = fakeUi(confirmPreselected);
  const { tool } = setupProfileTool(client);

  const result = await tool({ adapter: "claude", model: "sonnet", startupOptions: { claude: { mode: "tui" } } }, fakeExtCtx(repository, ui));

  expect(result.isError).not.toBe(true);
  const register = calls.find((c) => c.method === "profile/register");
  expect((register!.params as { model: string }).model).toBe("claude-sonnet-5");
  // Never silently: the caller is told what their input became.
  expect(result.content.some((c) => "text" in c && c.text.includes("claude-sonnet-5"))).toBe(true);
  // The suggestion was resolved to the row it preselected, not accepted raw.
  expect(ui.calls[0]?.options).toContain("claude-sonnet-5");
  expect(ui.calls[0]?.options[ui.calls[0]!.initialIndex!]).toBe("claude-sonnet-5");
});

test("crew_profile treats an explicit model matching the configured one as a no-op success", async () => {
  const repository = tempRepo();
  writeRepoConfig(repository, '{"adapters":{"claude":{"model":"opus"}}}');
  const { client, calls } = fakeClient();
  const { tool } = setupProfileTool(client);

  const result = await tool({ adapter: "claude", model: "opus", startupOptions: { claude: { mode: "tui" } } }, fakeExtCtx(repository));

  expect(result.isError).not.toBe(true);
  const register = calls.find((c) => c.method === "profile/register");
  expect((register!.params as { model: string }).model).toBe("claude-opus-5");
});

// The original symptom: a hallucinating leader invents a model name.
// An explicit param that conflicts with an already-stored model must be
// refused, not silently applied and not silently dropped -- either would
// hide the disagreement from whoever is supposed to resolve it.
test("crew_profile refuses with a typed conflict error when an explicit model differs from the configured one, and never calls profile/register", async () => {
  const repository = tempRepo();
  writeRepoConfig(repository, '{"adapters":{"claude":{"model":"opus"}}}');
  const { client, calls } = fakeClient();
  const { tool } = setupProfileTool(client);

  const result = await tool({ adapter: "claude", model: "sonnet", startupOptions: { claude: { mode: "tui" } } }, fakeExtCtx(repository));

  expect(result.isError).toBe(true);
  expect((result.details as { code: string; configuredModel: string }).code).toBe("model-conflict");
  expect((result.details as { code: string; configuredModel: string }).configuredModel).toBe("opus");
  expect(calls.map((c) => c.method)).not.toContain("profile/register");
});

test("crew_profile falls back to the configured model when none is given explicitly", async () => {
  const repository = tempRepo();
  writeRepoConfig(repository, '{"adapters":{"claude":{"model":"opus"}}}');
  const { client, calls } = fakeClient();
  const { tool } = setupProfileTool(client);

  const result = await tool({ adapter: "claude", startupOptions: { claude: { mode: "tui" } } }, fakeExtCtx(repository));

  expect(result.isError).not.toBe(true);
  const register = calls.find((c) => c.method === "profile/register");
  expect((register!.params as { model: string }).model).toBe("opus");
});

test("crew_profile returns a typed model-not-configured error, and never calls profile/register, when neither is available", async () => {
  const repository = tempRepo();
  const { client, calls } = fakeClient();
  const { tool } = setupProfileTool(client);

  const result = await tool({ adapter: "claude", startupOptions: { claude: { mode: "tui" } } }, fakeExtCtx(repository));

  expect(result.isError).toBe(true);
  expect((result.details as { code: string }).code).toBe("model-not-configured");
  expect(calls.map((c) => c.method)).not.toContain("profile/register");
});

// A leader-supplied model must not change this outcome: with no configured
// model and no interactive UI to ask through, crew fails closed regardless
// of what (or whether) the leader suggested -- the ask, not the suggestion,
// is what's missing.
test("crew_profile returns a typed model-not-configured error even with an explicit model, when there is no UI to ask through", async () => {
  const repository = tempRepo();
  const { client, calls } = fakeClient();
  const { tool } = setupProfileTool(client);

  const result = await tool({ adapter: "claude", model: "claude-sonnet-4-6-20260215", startupOptions: { claude: { mode: "tui" } } }, fakeExtCtx(repository));

  expect(result.isError).toBe(true);
  expect((result.details as { code: string; reason: string }).reason).toBe("no-ui");
  expect(calls.map((c) => c.method)).not.toContain("profile/register");
});

// ------------------------------------------------------------- persistence

test("crew_profile persists an explicit model into the repo layer when none was configured", async () => {
  const repository = tempRepo();
  const { client } = fakeClient();
  const ui = fakeUi(confirmPreselected);
  const { tool } = setupProfileTool(client);

  await tool({ adapter: "claude", model: "sonnet", startupOptions: { claude: { mode: "tui" } } }, fakeExtCtx(repository, ui));

  const written = JSON.parse(readFileSync(join(repository, ".omp", "crew.json"), "utf8"));
  // The resolved id, not the spelling passed in: a persisted alias is a
  // durable value whose meaning the vendor can migrate.
  expect(written.adapters.claude.model).toBe("claude-sonnet-5");
});

test("crew_profile preserves existing keys when persisting", async () => {
  const repository = tempRepo();
  writeRepoConfig(repository, '{"approval":"auto","adapters":{"codex":{"model":"gpt-5"}}}');
  const { client } = fakeClient();
  const ui = fakeUi(confirmPreselected);
  const { tool } = setupProfileTool(client);

  await tool({ adapter: "claude", model: "sonnet", startupOptions: { claude: { mode: "tui" } } }, fakeExtCtx(repository, ui));

  const written = JSON.parse(readFileSync(join(repository, ".omp", "crew.json"), "utf8"));
  expect(written.approval).toBe("auto");
  expect(written.adapters.codex.model).toBe("gpt-5");
  expect(written.adapters.claude.model).toBe("claude-sonnet-5");
});

test("crew_profile never (re-)persists when the explicit model just matches what's already configured", async () => {
  const repository = tempRepo();
  const path = writeRepoConfig(repository, '{"adapters":{"claude":{"model":"opus"}}}');
  const before = readFileSync(path, "utf8");
  const { client } = fakeClient();
  const { tool } = setupProfileTool(client);

  await tool({ adapter: "claude", model: "opus", startupOptions: { claude: { mode: "tui" } } }, fakeExtCtx(repository));

  expect(readFileSync(path, "utf8")).toBe(before);
});

test("crew_profile never persists when profile/register itself failed", async () => {
  const repository = tempRepo();
  // `callOrchestration` shapes a thrown JsonRpcRemoteError into an
  // isError result -- a rejecting client is the real error path.
  const rejecting = {
    request: async () => {
      throw new JsonRpcRemoteError(-32602, "bad params", undefined);
    },
  } as unknown as CrewClient;
  const { tool } = setupProfileTool(rejecting);
  const ui = fakeUi(confirmPreselected);

  const result = await tool({ adapter: "claude", model: "sonnet", startupOptions: { claude: { mode: "tui" } } }, fakeExtCtx(repository, ui));

  expect(result.isError).toBe(true);
  expect(() => readFileSync(join(repository, ".omp", "crew.json"), "utf8")).toThrow();
});

test("crew_profile warns rather than throwing when persistence fails after a successful registration", async () => {
  const repository = tempRepo();
  // Registration is already durable by the time persistConfiguredModel
  // runs -- simulate a concurrent process corrupting the repo layer file
  // in the window between the read (resolveConfiguredModel, at the top
  // of execute) and the write, by mutating it as a side effect of the
  // mocked profile/register call itself.
  const racy = {
    request: async () => {
      mkdirSync(join(repository, ".omp"), { recursive: true });
      writeFileSync(join(repository, ".omp", "crew.json"), "{ not valid json");
      return { profileId: "profile-1", sequence: 1 };
    },
  } as unknown as CrewClient;
  const { tool } = setupProfileTool(racy);
  const ui = fakeUi(confirmPreselected);

  const result = await tool({ adapter: "claude", model: "sonnet", startupOptions: { claude: { mode: "tui" } } }, fakeExtCtx(repository, ui));

  expect(result.isError).not.toBe(true);
  expect(result.content.some((c) => "text" in c && c.text.includes("Warning"))).toBe(true);
});

// -------------------------------------------------------------- injection

test("crew_profile fills in mode: tui for a reserved adapter when the caller omits it", async () => {
  const repository = tempRepo();
  const { client, calls } = fakeClient();
  const ui = fakeUi(confirmPreselected);
  const { tool } = setupProfileTool(client);

  await tool({ adapter: "claude", model: "sonnet" }, fakeExtCtx(repository, ui));

  const register = calls.find((c) => c.method === "profile/register");
  const startupOptions = (register!.params as { startupOptions: Record<string, unknown> }).startupOptions;
  expect((startupOptions.claude as { mode: string }).mode).toBe("tui");
});

test("injectTuiMode never overrides an explicit mode, including an explicit headless", () => {
  expect(injectTuiMode("claude", { claude: { mode: "headless" } })).toEqual({ claude: { mode: "headless" } });
});

test("injectTuiMode leaves a non-reserved adapter's startup options untouched", () => {
  expect(injectTuiMode("terminalDegraded", { terminalDegraded: { backend: "tmux" } })).toEqual({ terminalDegraded: { backend: "tmux" } });
});

test("injectTuiMode preserves other keys already present on the reserved adapter's own options", () => {
  expect(injectTuiMode("claude", { claude: { permissionMode: "max" } })).toEqual({ claude: { permissionMode: "max", mode: "tui" } });
});

// ------------------------------------------------------------- resolution

test("`sol` reaches the daemon as gpt-5.6-sol -- the maintainer's example, end to end", async () => {
  // No alias table entry exists for bare `sol`. It resolves because it is
  // the only openai-codex id containing it, which is why crew does not own
  // a mapping for the shorthand that motivated the ticket -- and, since
  // nothing is configured for codex yet, it only preselects that row; the
  // dialog still decides.
  const repository = tempRepo();
  const { client, calls } = fakeClient();
  const ui = fakeUi(confirmPreselected);
  const { tool } = setupProfileTool(client);

  const result = await tool({ adapter: "codex", model: "sol" }, fakeExtCtx(repository, ui));

  expect(result.isError).not.toBe(true);
  expect((calls.find((c) => c.method === "profile/register")!.params as { model: string }).model).toBe("gpt-5.6-sol");
  expect(JSON.parse(readFileSync(join(repository, ".omp", "crew.json"), "utf8")).adapters.codex.model).toBe("gpt-5.6-sol");
});

test("a stored shorthand and an explicit canonical id are one model, not a conflict", async () => {
  // Resolution has to precede the conflict check for this to pass. Compare
  // the spellings and a correct call is refused with an error telling the
  // user to go edit a crew.json that is already right.
  const repository = tempRepo();
  writeRepoConfig(repository, '{"adapters":{"claude":{"model":"opus"}}}');
  const { client, calls } = fakeClient();
  const { tool } = setupProfileTool(client);

  const result = await tool({ adapter: "claude", model: "claude-opus-5" }, fakeExtCtx(repository));

  expect(result.isError).not.toBe(true);
  expect((calls.find((c) => c.method === "profile/register")!.params as { model: string }).model).toBe("claude-opus-5");
});

// Ambiguity is a `decideModel` concern, reachable now only through the
// configured branch (an explicit override compared against a stored
// value) -- with nothing configured, an ambiguous suggestion just fails to
// preselect a dialog row (covered below) rather than refusing outright.
test("an ambiguous model is refused by name, and never registered, when it conflicts with an already-configured one", async () => {
  const repository = tempRepo();
  writeRepoConfig(repository, '{"adapters":{"codex":{"model":"gpt-5.5"}}}');
  const { client, calls } = fakeClient();
  const { tool } = setupProfileTool(client);

  const result = await tool({ adapter: "codex", model: "gpt-5.6-" }, fakeExtCtx(repository));

  expect(result.isError).toBe(true);
  const details = result.details as { code: string; candidates: string[] };
  expect(details.code).toBe("model-ambiguous");
  expect(details.candidates).toEqual(["gpt-5.6-luna", "gpt-5.6-sol", "gpt-5.6-terra"]);
  expect(calls.map((c) => c.method)).not.toContain("profile/register");
});

// The ticket's actual symptom, closed a second way: an invented dated id
// can no longer become the repository's durable answer at all, because it
// can't even be typed into existence -- with nothing configured, a name
// that isn't a real model just fails to preselect anything, and the human
// still has to pick one of the real options shown.
test("an invented model name preselects nothing in the dialog, and the human's real pick -- not the suggestion -- is what registers and persists", async () => {
  const repository = tempRepo();
  const { client, calls } = fakeClient();
  const ui = fakeUi(() => "claude-opus-5");
  const { tool } = setupProfileTool(client);

  const result = await tool({ adapter: "claude", model: "claude-sonnet-4-6-20260215", startupOptions: { claude: { mode: "tui" } } }, fakeExtCtx(repository, ui));

  expect(result.isError).not.toBe(true);
  expect(ui.calls[0]?.initialIndex).toBeUndefined();
  expect((calls.find((c) => c.method === "profile/register")!.params as { model: string }).model).toBe("claude-opus-5");
  expect(JSON.parse(readFileSync(join(repository, ".omp", "crew.json"), "utf8")).adapters.claude.model).toBe("claude-opus-5");
});

test("catalogue unavailable for the provider -- the dialog offers the vendor's own family table instead, and says so", async () => {
  // `gpt-5.6` is codex's own documented alias for `gpt-5.6-sol` -- the
  // suggestion resolves through the alias table even with the catalogue
  // down, so it still preselects the matching family-table row.
  const repository = tempRepo();
  const { client, calls } = fakeClient();
  const ui = fakeUi(confirmPreselected);
  const { tool } = setupProfileTool(client, NO_CATALOGUE);

  const result = await tool({ adapter: "codex", model: "gpt-5.6" }, fakeExtCtx(repository, ui));

  expect(result.isError).not.toBe(true);
  expect((calls.find((c) => c.method === "profile/register")!.params as { model: string }).model).toBe("gpt-5.6-sol");
  expect(ui.calls[0]?.options).toEqual(["gpt-5.6-sol"]);
  expect(ui.calls[0]?.title).toContain("vendor's own model family table");
  expect(result.content.some((c) => "text" in c && c.text.includes("family table"))).toBe(true);
  expect(JSON.parse(readFileSync(join(repository, ".omp", "crew.json"), "utf8")).adapters.codex.model).toBe("gpt-5.6-sol");
});

test("a local alias still resolves when the catalogue is unreachable, preselects, and is persisted once confirmed", async () => {
  const repository = tempRepo();
  const { client, calls } = fakeClient();
  const ui = fakeUi(confirmPreselected);
  const { tool } = setupProfileTool(client, NO_CATALOGUE);

  await tool({ adapter: "claude", model: "haiku" }, fakeExtCtx(repository, ui));

  expect((calls.find((c) => c.method === "profile/register")!.params as { model: string }).model).toBe("claude-haiku-4-5");
  expect(ui.calls[0]?.options[ui.calls[0]!.initialIndex!]).toBe("claude-haiku-4-5");
  expect(JSON.parse(readFileSync(join(repository, ".omp", "crew.json"), "utf8")).adapters.claude.model).toBe("claude-haiku-4-5");
});

test("dialog timeout returns a typed model-not-configured error, and never calls profile/register", async () => {
  const repository = tempRepo();
  const { client, calls } = fakeClient();
  const ui = fakeUi(() => undefined); // simulates extCtx.ui.select resolving undefined on timeout
  const { tool } = setupProfileTool(client);

  const result = await tool({ adapter: "claude", model: "sonnet" }, fakeExtCtx(repository, ui));

  expect(result.isError).toBe(true);
  expect((result.details as { code: string; reason: string }).reason).toBe("dialog-timeout");
  expect(calls.map((c) => c.method)).not.toContain("profile/register");
  expect(() => readFileSync(join(repository, ".omp", "crew.json"), "utf8")).toThrow();
});

test("a leader-supplied model never bypasses the dialog -- the pick wins even when it differs from the suggestion", async () => {
  const repository = tempRepo();
  const { client, calls } = fakeClient();
  // The leader suggests opus; the human picks sonnet instead.
  const ui = fakeUi(() => "claude-sonnet-5");
  const { tool } = setupProfileTool(client);

  const result = await tool({ adapter: "claude", model: "opus" }, fakeExtCtx(repository, ui));

  expect(result.isError).not.toBe(true);
  // The suggestion was still preselected -- the dialog just wasn't bound to it.
  expect(ui.calls[0]?.options[ui.calls[0]!.initialIndex!]).toBe("claude-opus-5");
  expect((calls.find((c) => c.method === "profile/register")!.params as { model: string }).model).toBe("claude-sonnet-5");
  expect(JSON.parse(readFileSync(join(repository, ".omp", "crew.json"), "utf8")).adapters.claude.model).toBe("claude-sonnet-5");
});

test("an adapter omp does not catalogue behaves exactly as it did before", async () => {
  // ompRpc has no provider and no alias source, so resolution could only
  // annotate a name nothing checked. Withholding persistence there would
  // make the adapter unusable across sessions for no gain.
  const repository = tempRepo();
  const { client, calls } = fakeClient();
  const { tool } = setupProfileTool(client);

  const result = await tool({ adapter: "ompRpc", model: "whatever-omp-calls-it" }, fakeExtCtx(repository));

  expect(result.isError).not.toBe(true);
  expect((calls.find((c) => c.method === "profile/register")!.params as { model: string }).model).toBe("whatever-omp-calls-it");
  // Keyed `omp`, not `ompRpc`: the config section name and the wire adapter
  // name differ for this one adapter, deliberately (see CONFIG_KEY_FOR_ADAPTER
  // in crew-config.ts, and crew.rs's RESERVED_ADAPTER_CONFIG_KEYS).
  expect(JSON.parse(readFileSync(join(repository, ".omp", "crew.json"), "utf8")).adapters.omp.model).toBe("whatever-omp-calls-it");
  expect(result.content.some((c) => "text" in c && c.text.includes("UNVERIFIED"))).toBe(false);
});

test("a model taken from crew.json is used exactly as recorded, not re-resolved", async () => {
  // Deliberate: re-resolving a stored value would turn a shorthand that has
  // always worked into an ambiguity error on a call that passed no model at
  // all -- a call the leader cannot correct by changing its own input.
  const repository = tempRepo();
  writeRepoConfig(repository, '{"adapters":{"claude":{"model":"haiku"}}}');
  const { client, calls } = fakeClient();
  const { tool } = setupProfileTool(client);

  const result = await tool({ adapter: "claude" }, fakeExtCtx(repository));

  expect(result.isError).not.toBe(true);
  expect((calls.find((c) => c.method === "profile/register")!.params as { model: string }).model).toBe("haiku");
});
