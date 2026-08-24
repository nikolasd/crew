import { expect, test } from "bun:test";
import schema from "../schema/crew.schema.json" with { type: "json" };
import type { EventEnvelope } from "./generated/EventEnvelope";
import type { InitializeParams } from "./generated/InitializeParams";
import { validateEventEnvelope } from "./validate";

test("schema is draft 2020-12", () => {
  expect(schema.$schema).toBe("https://json-schema.org/draft/2020-12/schema");
});

test("generated type accepts the golden initialize request", async () => {
  const value = (await Bun.file("fixtures/protocol/initialize.request.json").json()) as InitializeParams;
  expect(value.client.name).toBe("@nikolasd/crew");
});

// Smoke check for the crew v2 `RuntimeEvent` additions (WP6): the
// canonical schema accepts a well-formed `WorkerTimeout` event envelope
// and rejects one carrying an unknown field, the same way every other
// event variant is guarded by `deny_unknown_fields`.
test("a WorkerTimeout event envelope validates against the canonical schema", () => {
  const envelope: EventEnvelope = {
    sequence: 1,
    timestamp: "2026-01-01T00:00:00Z",
    projectId: "00000000-0000-7000-8000-000000000001",
    taskId: "00000000-0000-7000-8000-000000000002",
    workerId: "00000000-0000-7000-8000-000000000003",
    runId: "00000000-0000-7000-8000-000000000004",
    parentWorkerId: null,
    source: "runtime",
    event: {
      type: "workerTimeout",
      payload: {
        runId: "00000000-0000-7000-8000-000000000004",
        taskId: "00000000-0000-7000-8000-000000000002",
        workerId: "00000000-0000-7000-8000-000000000003",
        kind: "inactivity",
        sinceMs: 90_000,
      },
    },
    vendorEventRef: null,
  };
  expect(validateEventEnvelope(envelope)).toBe(true);

  const withUnknownField = {
    ...envelope,
    event: { ...envelope.event, payload: { ...(envelope.event as { payload: object }).payload, unexpected: true } },
  };
  expect(validateEventEnvelope(withUnknownField)).toBe(false);
});
