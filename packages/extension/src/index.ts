// The `@nikolasd/crew` OMP extension entry point. Registers `crew_health`
// (an LLM-callable tool), `/crew-status` (a slash command),
// `crew_doctor`/`/crew-doctor`, `crew_install`/
// `/crew-install`, the `/crew` monitor, and every deterministic
// orchestration tool (`crew_task`, `crew_worker`, `crew_profile`,
// `crew_run`, `crew_workspace`, `crew_artifact`, `crew_child`,
// `crew_violation`, `crew_message`, `crew_approval`,
// `crew_reconcile`). All share the single cached-client path: OMP loading
// this extension starts or reconnects to the per-repository `crewd` runtime
// once per session, and every tool reuses that connection.

import type { ExtensionAPI, ExtensionCommandContext, ExtensionContext } from "@oh-my-pi/pi-coding-agent";
import { TASK_SUBAGENT_EVENT_CHANNEL, TASK_SUBAGENT_LIFECYCLE_CHANNEL, TASK_SUBAGENT_PROGRESS_CHANNEL, type SubagentEventPayload, type SubagentLifecyclePayload, type SubagentProgressPayload } from "@oh-my-pi/pi-coding-agent/task";

import type { CrewClient } from "./client";
import { buildStatusContext } from "./context";
import { normalizeEventPayload, normalizeLifecyclePayload, normalizeProgressPayload } from "./omp-native/events";
import { OMP_NATIVE_FACT_ENTRY_TYPE, persistedCorrelations, persistedFacts, type SessionEntryLike } from "./omp-native/persistence";
import { OmpNativeReconciler, createOmpProcessEpoch, reconcileAcrossRestart, reconcileWithRuntime } from "./omp-native/reconcile";
import { resolveClient, getRuntimeStatus, type GetRuntimeStatusContext } from "./status";
import { runDoctorCommand, buildDoctorContext, type DoctorContext } from "./doctor";
import { runConfigCommand, type ConfigDocument, type ConfigRequest } from "./config";
import { installRuntimeForEnv } from "./install";
import { registerOrchestrationTools } from "./tools";
import { registerMonitor } from "./monitor/controller";
import type { ManagementSubcommand } from "./monitor/controller";

const TOOL_NAME = "crew_health";
const COMMAND_NAME = "crew-status";
const STATUS_DESCRIPTION = "Use to verify the Crew runtime is reachable and healthy before orchestration operations. Returns connection status, runtime identity, and binary source. Call this if you're unsure the daemon is running, or after a connection failure.";
const INSTALL_TOOL_NAME = "crew_install";

export default function crewExtension(pi: ExtensionAPI): void {
  // Cached per extension instance (one per OMP session), closed on shutdown.
  let cachedClient: CrewClient | undefined;

  function statusContextFor(extCtx: ExtensionContext): GetRuntimeStatusContext {
    const { ensureRuntimeOptions } = buildStatusContext({ cwd: extCtx.cwd, sessionId: extCtx.sessionManager.getSessionId() });
    return {
      ensureRuntimeOptions,
      cache: {
        get: () => cachedClient,
        set: (client) => {
          cachedClient = client;
        },
      },
    };
  }

  /**
   * Resolves the cached client for `cwd`, connecting (or spawning) the
   * repository's runtime on first use. Reuses the cached connection while
   * its socket is still open; a closed cached client is replaced so a daemon
   * idle-exit or socket failure repairs itself on the next call.
   */
  async function getClient(extCtx: ExtensionContext): Promise<CrewClient> {
    return resolveClient(statusContextFor(extCtx));
  }

  pi.registerTool({
    name: TOOL_NAME,
    label: "Crew Health",
    description: STATUS_DESCRIPTION,
    parameters: pi.zod.object({}),
    async execute(_toolCallId, _params, _signal, _onUpdate, extCtx) {
      return getRuntimeStatus(statusContextFor(extCtx));
    },
  });

  pi.registerCommand(COMMAND_NAME, {
    description: STATUS_DESCRIPTION,
    handler: async (_args, ctx) => emitResult(ctx, await healthResult(ctx)),
  });

  registerOrchestrationTools(pi, { getClient });
  /**
   * Context builder for the doctor command: resolves the crewd binary path
   * and repository state for direct CLI invocation.
   */
  function doctorContextFor(cwd: ExtensionContext["cwd"]): DoctorContext {
    return buildDoctorContext(cwd);
  }

  type CommandResult = { text: string; isError: boolean };

  function blocksToResult(result: { content: Array<{ text: string }>; isError?: boolean }): CommandResult {
    return { text: result.content.map((block) => block.text).join("\n"), isError: result.isError === true };
  }

  function emitResult(ctx: ExtensionCommandContext, result: CommandResult): void {
    if (!ctx.hasUI) {
      console.log(result.text);
    } else {
      ctx.ui.notify(result.text, result.isError ? "error" : "info");
    }
  }

  async function healthResult(extCtx: ExtensionContext): Promise<CommandResult> {
    return blocksToResult(await getRuntimeStatus(statusContextFor(extCtx)));
  }

  async function doctorResult(cwd: string): Promise<CommandResult> {
    return blocksToResult(await runDoctorCommand(doctorContextFor(cwd)));
  }

  async function configResult(args: string, cwd: string): Promise<CommandResult> {
    const [op = "path", ...rest] = args.trim().split(/\s+/).filter(Boolean);
    if (op !== "path" && op !== "print" && op !== "init") {
      return { text: `Unknown operation ${op}. Usage: /crew config [path | print [effective|defaults|schema] | init [global] [force]]`, isError: true };
    }
    const request = {
      op,
      repository: cwd,
      ...(op === "print" ? { document: (rest[0] as ConfigDocument) ?? "effective" } : {}),
      ...(op === "init" ? { global: rest.includes("global"), force: rest.includes("force") } : {}),
    } as ConfigRequest;
    const { crewdPath } = doctorContextFor(cwd);
    return blocksToResult(await runConfigCommand({ crewdPath, repository: cwd }, request));
  }

  registerMonitor(pi, {
    getClient,
    management: new Map<string, ManagementSubcommand>([
      ["health", { description: "Runtime health: connects to or spawns the daemon", run: async (_args, ctx) => healthResult(ctx) }],
      ["doctor", { description: "Diagnostics that work with no live daemon", run: async (_args, ctx) => doctorResult(ctx.cwd) }],
      ["config", { description: "Inspect or scaffold crew.json", hint: "[path | print [effective|defaults|schema] | init [global] [force]]", run: async (args, ctx) => configResult(args, ctx.cwd) }],
    ]),
  });

  pi.registerTool({
    name: "crew_doctor",
    label: "Crew Doctor",
    description: "Use for deep diagnostics when crew_health fails or the runtime is unreachable. Runs checks without connecting to a running daemon -- verifies database, state directory, rollout gates, and configuration. Use when the runtime won't start or status reports errors.",
    parameters: pi.zod.object({}),
    async execute(_toolCallId, _params, _signal, _onUpdate, ctx) {
      return runDoctorCommand(doctorContextFor(ctx.cwd));
    },
  });

  pi.registerCommand("crew-doctor", {
    description: "Run diagnostic checks on the Crew runtime state and configuration.",
    handler: async (_args, ctx) => emitResult(ctx, await doctorResult(ctx.cwd)),
  });

  const configParams = pi.zod.object({
    op: pi.zod.enum(["path", "print", "init"]).describe("Which config operation to perform."),
    document: pi.zod.enum(["effective", "defaults", "schema"]).optional().describe("For op 'print': which document to emit. Defaults to 'effective'."),
    global: pi.zod.boolean().optional().describe("For op 'init': write ~/.omp/crew.json instead of the repository layer."),
    force: pi.zod.boolean().optional().describe("For op 'init': overwrite an existing crew.json. Without it, an existing file is left untouched."),
  });

  pi.registerTool({
    name: "crew_config",
    label: "Crew Config",
    description:
      "Use to inspect or scaffold Crew's crew.json configuration. op: 'path' lists which config layers exist and in what precedence order; op: 'print' shows a document (document: 'effective' -- the merged config actually in force, the default -- or 'defaults' / 'schema'); op: 'init' writes a starter crew.json plus its JSON Schema, into <repo>/.omp by default or ~/.omp with global: true. init writes a full snapshot of today's defaults, so every key in it becomes an override; it refuses to overwrite an existing file unless force is true. Use op: 'print' with document: 'effective' when you need to know what setting a run will actually use.",
    parameters: configParams,
    // `init` writes files the operator owns; reads stay cheap.
    approval: (args) => (typeof args === "object" && args !== null && "op" in args && args.op === "init" ? "write" : "read"),
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const { crewdPath } = doctorContextFor(extCtx.cwd);
      const request = {
        op: input.op,
        repository: extCtx.cwd,
        ...(input.document !== undefined ? { document: input.document as ConfigDocument } : {}),
        ...(input.global !== undefined ? { global: input.global } : {}),
        ...(input.force !== undefined ? { force: input.force } : {}),
      } as ConfigRequest;
      return runConfigCommand({ crewdPath, repository: extCtx.cwd }, request);
    },
  });

  pi.registerCommand("crew-config", {
    description: "Inspect or scaffold crew.json. Usage: /crew config [path | print [effective|defaults|schema] | init [global] [force]]",
    handler: async (args, ctx) => emitResult(ctx, await configResult(args, ctx.cwd)),
  });

  pi.registerTool({
    name: INSTALL_TOOL_NAME,
    label: "Crew Install",
    description:
      "Use to download and verify the crewd runtime binary for this platform. Call this when crew_health or any orchestration tool fails with code 'runtime-not-installed'. Downloads the GitHub release asset matching this extension's version, verifies its SHA-256 against the published manifest, and caches it under the Crew state root. nikolasd/crew is a private repository, so this needs read access to it: set GITHUB_TOKEN or GH_TOKEN, or run `gh auth login` locally.",
    parameters: pi.zod.object({}),
    approval: "exec",
    async execute(_toolCallId, _params, _signal, _onUpdate) {
      return installRuntimeForEnv(process.env);
    },
  });

  pi.registerCommand("crew-install", {
    description: "Download and verify the crewd runtime binary for this platform.",
    handler: async (_args, ctx) => {
      const result = await installRuntimeForEnv(process.env);
      const text = result.content.map((block) => block.text).join("\n");
      if (!ctx.hasUI) {
        console.log(text);
      } else {
        ctx.ui.notify(text, result.isError ? "error" : "info");
      }
    },
  });

  // OMP-native subagent lifecycle mirroring: one epoch per extension
  // process, normalized facts recorded by the reconciler, listeners
  // registered on session_start and removed on session_shutdown.
  //
  // `onChange` persists each committed fact into OMP's own session log, so
  // the next process can tell a run that ended from one whose OMP process
  // vanished mid-flight. Without persistence `reconcileAcrossRestart`
  // would always receive an empty list and could never transition anything
  // to `lost`.
  const ompProcessEpoch = createOmpProcessEpoch();
  const reconciler = new OmpNativeReconciler((fact) => {
    try {
      pi.appendEntry(OMP_NATIVE_FACT_ENTRY_TYPE, { ...fact });
    } catch (err) {
      // Persistence is best-effort telemetry: losing one entry degrades a
      // later restart's classification, and must never break the live
      // session that produced the fact.
      pi.logger.warn("crew omp-native: failed to persist fact", {
        error: err instanceof Error ? err.message : String(err),
      });
    }
  });
  let unsubscribers: Array<() => void> = [];

  pi.on("session_start", async (_payload, extCtx) => {
    unsubscribers = [
      // The event bus is untyped (`EventBus.on` receives `unknown`); these
      // three channels are SDK-internal and documented at
      // `@oh-my-pi/pi-coding-agent/task`, with no runtime schema exported to
      // validate against, so the cast is the pinned public contract itself.
      pi.events.on(TASK_SUBAGENT_LIFECYCLE_CHANNEL, (data) => {
        const payload = data as SubagentLifecyclePayload;
        reconciler.record(normalizeLifecyclePayload(payload, ompProcessEpoch, Date.now()));
      }),
      pi.events.on(TASK_SUBAGENT_PROGRESS_CHANNEL, (data) => {
        const payload = data as SubagentProgressPayload;
        reconciler.record(normalizeProgressPayload(payload, ompProcessEpoch, Date.now()));
      }),
      pi.events.on(TASK_SUBAGENT_EVENT_CHANNEL, (data) => {
        const payload = data as SubagentEventPayload;
        const fact = normalizeEventPayload(payload);
        if (fact !== undefined) {
          reconciler.record(fact);
        }
      }),
    ];

    await reconcilePriorProcess(extCtx);
  });

  /**
   * Settles what a prior OMP process left behind, before any of it can be
   * rendered as live:
   *
   * 1. every non-terminal fact from a foreign epoch becomes `lost`, and is
   *    re-persisted under this epoch so it is settled exactly once;
   * 2. every task a prior process correlated is rebound to this instance
   *    via `reconcile/omp`, which is the only way ownership transfers --
   *    the runtime exposes no way to enumerate owned tasks.
   *
   * Entirely non-fatal: the daemon may legitimately be absent when OMP
   * starts, and a failed reconciliation must never block activation.
   */
  async function reconcilePriorProcess(extCtx: ExtensionContext): Promise<void> {
    const entries = extCtx.sessionManager.getEntries() as SessionEntryLike[];

    const settled = reconcileAcrossRestart(persistedFacts(entries), ompProcessEpoch);
    for (const fact of settled) {
      if (fact.ompProcessEpoch === ompProcessEpoch && fact.status === "lost") {
        reconciler.record(fact);
      }
    }

    const correlations = persistedCorrelations(entries);
    if (correlations.length === 0) {
      return;
    }
    try {
      const client = await getClient(extCtx);
      for (const correlation of correlations) {
        try {
          await reconcileWithRuntime(client, correlation);
        } catch (err) {
          // A stale revision is the expected, benign case: another
          // instance already rebound this task.
          pi.logger.warn("crew omp-native: task reconciliation refused", {
            taskId: correlation.taskId,
            error: err instanceof Error ? err.message : String(err),
          });
        }
      }
    } catch (err) {
      pi.logger.warn("crew omp-native: runtime unavailable for reconciliation", {
        error: err instanceof Error ? err.message : String(err),
      });
    }
  }

  pi.on("session_shutdown", async () => {
    cachedClient?.close();
    cachedClient = undefined;
    for (const unsubscribe of unsubscribers) {
      unsubscribe();
    }
    unsubscribers = [];
    reconciler.dispose();
  });
}
