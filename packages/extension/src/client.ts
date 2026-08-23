// A validated JSON-RPC client for the Crew runtime socket.
//
// `CrewClient` connects to the runtime's per-repository Unix domain socket,
// performs the `initialize` handshake, and correlates requests to responses by
// a monotonically increasing string id. Validation boundary (R55): the
// JSON-RPC envelope of every inbound message and every event notification is
// schema-validated (Ajv) before it reaches caller code. Result payloads are
// schema-validated for every method with a canonical protocol result type
// (`RESULT_VALIDATORS`); every other method's result is structurally
// validated to be a JSON object, so a null/scalar/array result can never
// reach tool logic. Framing is newline-delimited JSON with a 4 MiB bootstrap
// hard limit, tightened to the negotiated `maxFrameBytes` in both directions
// once `initialize` succeeds.

import { createConnection, type Socket } from "node:net";

import {
  assertValid,
  ValidationError,
  validateApplyResult,
  validateArtifactFetchResult,
  validateArtifactListResult,
  validateEventEnvelope,
  validateEventEnvelopeArray,
  validateInitializeResult,
  validateInspectResult,
  validateJsonRpcErrorResponse,
  validateJsonRpcNotification,
  validateJsonRpcResponse,
  validatePolicyViolationListResult,
  validateRunResultResult,
  validateRuntimeStatus,
  validateWorkspaceInfo,
  type ValidateFunction,
} from "@nikolasd/batman-protocol/validate";
import type { EventEnvelope, InitializeParams, InitializeResult } from "@nikolasd/batman-protocol";

/** The 4 MiB bootstrap frame limit applied before `initialize` completes. */
const BOOTSTRAP_MAX_FRAME_BYTES = 4 * 1024 * 1024;

/** The method name of the runtime's event notifications. */
const EVENTS_EVENT_METHOD = "events/event";

/** Per-method result validators: every method whose result has a canonical
 *  protocol type is schema-validated here; the rest get the structural
 *  object check in {@link CrewClient.request}. `initialize` is validated
 *  separately in {@link CrewClient.initialize}. */
const RESULT_VALIDATORS: Record<string, ValidateFunction> = {
  "runtime/status": validateRuntimeStatus,
  "artifact/list": validateArtifactListResult,
  "artifact/fetch": validateArtifactFetchResult,
  "workspace/inspect": validateInspectResult,
  "workspace/apply": validateApplyResult,
  "workspace/get": validateWorkspaceInfo,
  "policy/violation/list": validatePolicyViolationListResult,
  "run/result": validateRunResultResult,
};

/** Removes a subscription registered with {@link CrewClient.subscribe}. */
export type Unsubscribe = () => void;

/** Options for constructing a {@link CrewClient}. */
export interface CrewClientOptions {
  /** Filesystem path of the runtime's Unix domain socket. */
  socketPath: string;
}

/** A JSON-RPC error returned by the runtime. */
export class JsonRpcRemoteError extends Error {
  readonly code: number;
  readonly data: unknown;

  constructor(code: number, message: string, data: unknown) {
    super(message);
    this.name = "JsonRpcRemoteError";
    this.code = code;
    this.data = data;
  }
}

interface PendingRequest {
  readonly method: string;
  readonly resolve: (value: unknown) => void;
  readonly reject: (reason: Error) => void;
}

export { ValidationError };

export class CrewClient {
  #socket: Socket;
  #buffer = "";
  #maxFrameBytes = BOOTSTRAP_MAX_FRAME_BYTES;
  #nextId = 1;
  #initialized = false;
  #closed = false;
  #closeReason: Error | undefined;
  readonly #pending = new Map<string, PendingRequest>();
  readonly #subscribers = new Set<(event: EventEnvelope) => void>();
  readonly #ready: Promise<void>;

  constructor(options: CrewClientOptions) {
    this.#socket = createConnection({ path: options.socketPath });
    this.#socket.setEncoding("utf8");

    this.#ready = new Promise<void>((resolve, reject) => {
      this.#socket.once("connect", () => resolve());
      this.#socket.once("error", (err: Error) => reject(err));
    });

    this.#socket.on("data", (chunk: string) => this.#onData(chunk));
    this.#socket.on("close", () => this.#onClose());
    this.#socket.on("error", (err: Error) => this.#onError(err));
  }

  /**
   * Performs the `initialize` handshake and returns the negotiated result.
   * The returned `capabilities.maxFrameBytes` becomes the frame limit enforced
   * in both directions for the rest of the session.
   */
  async initialize(params: InitializeParams): Promise<InitializeResult> {
    const result = await this.#send("initialize", params);
    assertValid<InitializeResult>(validateInitializeResult, result, "initialize result");
    this.#maxFrameBytes = result.capabilities.maxFrameBytes;
    this.#initialized = true;
    return result;
  }

  /**
   * Sends a JSON-RPC request and resolves with its `result`. Methods with a
   * canonical protocol result type are schema-validated; every other result
   * must at least be a JSON object (events/replay's array is validated in
   * {@link CrewClient.subscribe} before this guard would see it).
   */
  async request(method: string, params?: unknown): Promise<unknown> {
    if (!this.#initialized && method !== "initialize") {
      throw new Error(`cannot call ${method} before initialize()`);
    }
    const result = await this.#send(method, params);
    const validator = RESULT_VALIDATORS[method];
    if (validator !== undefined) {
      assertValid(validator, result, `${method} result`);
    } else if (!isObject(result)) {
      throw new ValidationError(`${method} result`, [{ message: "result is not a JSON object" }]);
    }
    return result;
  }

  /**
   * Subscribes to runtime events. Committed events after `fromSequence` are
   * replayed to `onEvent`, then live events are delivered as they arrive.
   * Returns a function that cancels the subscription.
   */
  subscribe(fromSequence: number, onEvent: (event: EventEnvelope) => void): Unsubscribe {
    this.#subscribers.add(onEvent);

    void (async () => {
      try {
        const replayed = await this.#send("events/replay", { afterSequence: fromSequence });
        assertValid<EventEnvelope[]>(validateEventEnvelopeArray, replayed, "events/replay result");
        for (const event of replayed as EventEnvelope[]) {
          onEvent(event);
        }
        // The `{ "active": true }` ack is deliberately not validated: it is
        // the subscription trigger, not data, and this path already bypasses
        // request()'s guards the same way events/replay's array does above.
        await this.#send("events/subscribe", {});
      } catch (err) {
        // A failed subscription must not take down the process; surface it via
        // the socket error path instead.
        this.#socket.emit("error", err instanceof Error ? err : new Error(String(err)));
      }
    })();

    return () => {
      this.#subscribers.delete(onEvent);
    };
  }

  /** Closes the connection and rejects every outstanding request. */
  close(): void {
    if (this.#closed) {
      return;
    }
    this.#closed = true;
    this.#socket.destroy();
    this.#failPending(new Error("client closed"));
  }

  /**
   * True once the socket has closed or errored. A closed client rejects
   * every request, so callers holding a cached instance must re-resolve
   * rather than reuse it.
   */
  get isClosed(): boolean {
    return this.#closed;
  }

  #send(method: string, params: unknown): Promise<unknown> {
    return new Promise<unknown>((resolve, reject) => {
      if (this.#closed) {
        reject(this.#closeReason ?? new Error("client is closed"));
        return;
      }
      const id = String(this.#nextId++);
      const frame = JSON.stringify({ jsonrpc: "2.0", id, method, params });
      const frameBytes = Buffer.byteLength(frame, "utf8");

      if (frameBytes + 1 > this.#maxFrameBytes) {
        reject(new Error(`outbound frame of ${frameBytes + 1} bytes exceeds the negotiated maximum of ${this.#maxFrameBytes}`));
        return;
      }

      this.#pending.set(id, { method, resolve, reject });
      this.#socket.write(`${frame}\n`, (err) => {
        if (err) {
          this.#pending.delete(id);
          reject(err);
        }
      });
    });
  }

  #onData(chunk: string): void {
    this.#buffer += chunk;

    let newline = this.#buffer.indexOf("\n");
    while (newline !== -1) {
      const line = this.#buffer.slice(0, newline);
      this.#buffer = this.#buffer.slice(newline + 1);
      if (line.length > 0) {
        // Enforce the negotiated cap on every complete frame *before*
        // parsing/validating/dispatching it -- a fully-buffered frame that
        // already exceeds the limit must never reach caller code.
        const lineBytes = Buffer.byteLength(line, "utf8");
        if (lineBytes + 1 > this.#maxFrameBytes) {
          this.#onError(new Error(`inbound frame of ${lineBytes + 1} bytes exceeds the negotiated maximum of ${this.#maxFrameBytes}`));
          return;
        }
        this.#handleLine(line);
      }
      newline = this.#buffer.indexOf("\n");
    }

    // An unterminated frame that already exceeds the limit can never become
    // valid: fail closed rather than buffer without bound.
    if (Buffer.byteLength(this.#buffer, "utf8") > this.#maxFrameBytes) {
      this.#onError(new Error(`inbound frame exceeds the ${this.#maxFrameBytes}-byte maximum with no frame boundary`));
    }
  }

  #handleLine(line: string): void {
    let message: unknown;
    try {
      message = JSON.parse(line);
    } catch {
      this.#onError(new Error("received a frame that is not valid JSON"));
      return;
    }

    if (!isObject(message)) {
      this.#onError(new Error("received a non-object JSON-RPC message"));
      return;
    }

    // A notification (a method call with no id) carries a runtime event.
    if (message.id === undefined && typeof message.method === "string") {
      this.#handleNotification(message);
      return;
    }

    this.#handleResponse(message);
  }

  #handleNotification(message: Record<string, unknown>): void {
    try {
      assertValid(validateJsonRpcNotification, message, "notification envelope");
    } catch (err) {
      this.#onError(err instanceof Error ? err : new Error(String(err)));
      return;
    }

    if (message.method === EVENTS_EVENT_METHOD) {
      const params = message.params;
      try {
        assertValid<EventEnvelope>(validateEventEnvelope, params, "event notification");
      } catch (err) {
        this.#onError(err instanceof Error ? err : new Error(String(err)));
        return;
      }
      for (const subscriber of this.#subscribers) {
        subscriber(params as EventEnvelope);
      }
    }
  }

  #handleResponse(message: Record<string, unknown>): void {
    const id = typeof message.id === "string" ? message.id : String(message.id);
    const pending = this.#pending.get(id);
    if (pending === undefined) {
      // A response we never asked for: treat as a protocol violation.
      this.#onError(new Error(`received a response for unknown id ${id}`));
      return;
    }
    this.#pending.delete(id);

    if ("error" in message) {
      try {
        assertValid(validateJsonRpcErrorResponse, message, "error response envelope");
      } catch (err) {
        pending.reject(err instanceof Error ? err : new Error(String(err)));
        return;
      }
      const error = message.error as { code: number; message: string; data?: unknown };
      pending.reject(new JsonRpcRemoteError(error.code, error.message, error.data));
      return;
    }

    try {
      assertValid(validateJsonRpcResponse, message, "success response envelope");
    } catch (err) {
      pending.reject(err instanceof Error ? err : new Error(String(err)));
      return;
    }
    pending.resolve(message.result);
  }

  #onError(err: Error): void {
    this.#closeReason ??= err;
    this.#failPending(err);
    if (!this.#closed) {
      this.#closed = true;
      this.#socket.destroy();
    }
  }

  #onClose(): void {
    this.#closed = true;
    this.#failPending(this.#closeReason ?? new Error("connection closed by runtime"));
  }

  #failPending(reason: Error): void {
    for (const pending of this.#pending.values()) {
      pending.reject(reason);
    }
    this.#pending.clear();
  }

  /** Resolves once the socket has connected (or rejects on connect error). */
  whenConnected(): Promise<void> {
    return this.#ready;
  }
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
