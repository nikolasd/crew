// Ajv 2020 validators compiled once from the canonical BATMAN JSON Schema.
//
// The schema is generated from `batman-protocol` (see `bun run generate`);
// every `$def` uses `additionalProperties: false`, so validating an inbound
// payload against its `$def` rejects any unknown field. Validators are
// compiled a single time at module load and reused for every message.

import Ajv2020, { type ValidateFunction } from "ajv/dist/2020";
import schema from "../schema/batman.schema.json" with { type: "json" };

/** The Ajv validate-function type every exported validator conforms to;
 *  re-exported so consumers can type validator tables without importing
 *  ajv directly. */
export type { ValidateFunction };

/** The `$id` under which the whole schema document is registered, so
 * individual `$def`s can be retrieved (and cross-referenced) by JSON pointer. */
const SCHEMA_ID = "https://schema.batman.satorianalytics.com/batman.schema.json";

const ajv = new Ajv2020({
  strict: true,
  allErrors: true,
  coerceTypes: false,
  removeAdditional: false,
  useDefaults: false,
});

// The schema carries Rust numeric-width `format` hints (`uint32`,
// `int64`, `double`, ...). These are not JSON validation constraints --
// each numeric field already carries `minimum`/`maximum` bounds where
// applicable -- so, rather than relax `strict`, register them as
// always-passing formats. This is the single documented relaxation
// required by the schema's keyword set. `float`/`double` cover `f32`/
// `f64` fields (e.g. `AdapterUsageEvent.costUsd`).
for (const format of ["int16", "uint16", "int32", "uint32", "int64", "uint64", "float", "double"]) {
  ajv.addFormat(format, true);
}

ajv.addSchema({ ...schema, $id: SCHEMA_ID });

function def(name: string): ValidateFunction {
  const validate = ajv.getSchema(`${SCHEMA_ID}#/$defs/${name}`);
  if (validate === undefined) {
    throw new Error(`schema is missing the expected $def: ${name}`);
  }
  return validate as ValidateFunction;
}

/** Validates a successful `initialize` result payload. */
export const validateInitializeResult = def("InitializeResult");
/** Validates a `runtime/status` result payload. */
export const validateRuntimeStatus = def("RuntimeStatus");
/** Validates an `artifact/list` result payload. */
export const validateArtifactListResult = def("ArtifactListResult");
/** Validates an `artifact/fetch` result payload. */
export const validateArtifactFetchResult = def("ArtifactFetchResult");
/** Validates a `workspace/inspect` result payload. */
export const validateInspectResult = def("InspectResult");
/** Validates a `workspace/apply` result payload. */
export const validateApplyResult = def("ApplyResult");
/** Validates a `workspace/get` result payload. */
export const validateWorkspaceInfo = def("WorkspaceInfo");
/** Validates a `policy/violation/list` result payload. */
export const validatePolicyViolationListResult = def("PolicyViolationListResult");
/** Validates a single durable event envelope. */
export const validateEventEnvelope = def("EventEnvelope");
/** Validates a JSON-RPC success response envelope. */
export const validateJsonRpcResponse = def("JsonRpcResponse");
/** Validates a JSON-RPC error response envelope. */
export const validateJsonRpcErrorResponse = def("JsonRpcErrorResponse");
/** Validates a JSON-RPC notification envelope. */
export const validateJsonRpcNotification = def("JsonRpcNotification");
/** Validates a `run/result` result payload. */
export const validateRunResultResult = def("RunResultResult");
/** Validates a `plan/propose` result payload. */
export const validatePlanProposeResult = def("PlanProposeResult");
/** Validates a `plan/decide` result payload. */
export const validatePlanDecideResult = def("PlanDecideResult");
/** Validates a `plan/get` result payload. */
export const validatePlanGetResult = def("PlanGetResult");
/** Validates a `run/timeoutAck` result payload. */
export const validateRunTimeoutAckResult = def("RunTimeoutAckResult");

/** Validates the array of event envelopes returned by `events/replay`. */
export const validateEventEnvelopeArray = ajv.compile({
  $id: "https://schema.batman.satorianalytics.com/event-envelope-array.json",
  type: "array",
  items: { $ref: `${SCHEMA_ID}#/$defs/EventEnvelope` },
});

/** Thrown when an inbound payload fails schema validation. */
export class ValidationError extends Error {
  readonly what: string;
  readonly errors: unknown;

  constructor(what: string, errors: unknown) {
    super(`${what} failed schema validation: ${JSON.stringify(errors)}`);
    this.name = "ValidationError";
    this.what = what;
    this.errors = errors;
  }
}

/**
 * Runs `validate` over `data`, throwing {@link ValidationError} if it fails.
 * The generic parameter narrows `data` to `T` on success so callers can treat
 * a validated payload as its wire type.
 */
export function assertValid<T>(validate: ValidateFunction, data: unknown, what: string): asserts data is T {
  if (!validate(data)) {
    throw new ValidationError(what, validate.errors ?? null);
  }
}
